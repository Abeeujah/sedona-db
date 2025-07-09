use core::fmt;
use std::{mem::transmute, sync::Arc};

use arrow_array::{ArrayRef, Float64Array, RecordBatch};
use datafusion_common::{
    utils::proxy::VecAllocExt, DataFusionError, JoinSide, Result, ScalarValue,
};
use datafusion_expr::ColumnarValue;
use datafusion_physical_expr::PhysicalExpr;
use geo_generic_alg::BoundingRect;
use geo_types::Rect;
use sedona_functions::executor::IterGeo;
use sedona_schema::datatypes::SedonaType;
use wkb::reader::Wkb;

use sedona_common::option::SpatialJoinOptions;

use crate::spatial_predicate::{DistancePredicate, RelationPredicate, SpatialPredicate};

/// Operand evaluator is for evaluating the operands of a spatial predicate. It can be a distance
/// operand evaluator or a relation operand evaluator.
pub(crate) trait OperandEvaluator: fmt::Debug + Send + Sync {
    /// Evaluate the spatial predicate operand on the build side.
    fn evaluate_build(&self, batch: &RecordBatch) -> Result<EvaluatedGeometryArray> {
        let geom_expr = self.build_side_expr()?;
        evaluate_with_rects(batch, &geom_expr)
    }

    /// Evaluate the spatial predicate operand on the probe side.
    fn evaluate_probe(&self, batch: &RecordBatch) -> Result<EvaluatedGeometryArray> {
        let geom_expr = self.probe_side_expr()?;
        evaluate_with_rects(batch, &geom_expr)
    }

    /// Resolve the distance operand for a given row.
    fn resolve_distance(
        &self,
        _build_distance: &Option<ColumnarValue>,
        _probe_distance: &Option<ColumnarValue>,
        _row_idx: usize,
    ) -> Result<Option<f64>> {
        Ok(None)
    }

    /// Get the expression for the build side.
    fn build_side_expr(&self) -> Result<Arc<dyn PhysicalExpr>>;

    /// Get the expression for the probe side.
    fn probe_side_expr(&self) -> Result<Arc<dyn PhysicalExpr>>;
}

/// Create a spatial predicate evaluator for the spatial predicate.
pub(crate) fn create_operand_evaluator(
    predicate: &SpatialPredicate,
    options: SpatialJoinOptions,
) -> Arc<dyn OperandEvaluator> {
    match predicate {
        SpatialPredicate::Distance(predicate) => {
            Arc::new(DistanceOperandEvaluator::new(predicate.clone(), options))
        }
        SpatialPredicate::Relation(predicate) => {
            Arc::new(RelationOperandEvaluator::new(predicate.clone(), options))
        }
    }
}

/// Result of evaluating a geometry batch.
pub(crate) struct EvaluatedGeometryArray {
    /// The array of geometries produced by evaluating the geometry expression.
    pub geometry_array: ArrayRef,
    /// The rects of the geometries in the geometry array. Each geometry could be covered by a collection
    /// of multiple rects. The first element of the tuple is the index of the geometry in the geometry array.
    /// This array is guaranteed to be sorted by the index of the geometry.
    pub rects: Vec<(usize, Rect)>,
    /// The distance value produced by evaluating the distance expression.
    pub distance: Option<ColumnarValue>,
    /// The array of WKBs of the geometries unwrapped from the geometry array. It is a reference to
    /// some of the columns of the `geometry_array`. We need to keep it here since the WKB values reference
    /// buffers inside the geometry array, but we'll only allow accessing Wkb<'a> where 'a is the lifetime of
    /// the GeometryBatchResult to make the interfaces safe.
    #[allow(dead_code)]
    wkb_array: ArrayRef,
    /// WKBs of the geometries in `wkb_array`. The wkb values reference buffers inside the geometry array,
    /// but we'll only allow accessing Wkb<'a> where 'a is the lifetime of the GeometryBatchResult to make
    /// the interfaces safe. The buffers in `wkb_array` are allocated on the heap and won't be moved when
    /// the GeometryBatchResult is moved, so we don't need to worry about pinning.
    wkbs: Vec<Option<Wkb<'static>>>,
}

impl EvaluatedGeometryArray {
    pub fn try_new(geometry_array: ArrayRef) -> Result<Self> {
        let num_rows = geometry_array.len();
        let mut rect_vec = Vec::with_capacity(num_rows);
        let sedona_type: SedonaType = geometry_array.data_type().try_into()?;
        let wkb_array = sedona_type.unwrap_array(&geometry_array)?;
        let mut wkbs = Vec::with_capacity(num_rows);
        wkb_array.iter_as_wkb(&sedona_type, num_rows, |idx, wkb_opt| {
            if let Some(wkb) = &wkb_opt {
                if let Some(rect) = wkb.bounding_rect() {
                    rect_vec.push((idx, rect));
                }
            }
            wkbs.push(wkb_opt);
            Ok(())
        })?;

        // Safety: The wkbs must reference buffers inside the `wkb_array`.
        let wkbs = wkbs
            .into_iter()
            .map(|wkb| wkb.map(|wkb| unsafe { transmute(wkb) }))
            .collect();
        Ok(Self {
            geometry_array,
            rects: rect_vec,
            distance: None,
            wkb_array,
            wkbs,
        })
    }

    /// Get the WKBs of the geometries in the geometry array.
    pub fn wkbs(&self) -> &Vec<Option<Wkb<'_>>> {
        // The returned WKBs are guaranteed to be valid for the lifetime of the GeometryBatchResult,
        // because the WKBs reference buffers inside `geometry_array`, which is guaranteed to be valid
        // for the lifetime of the GeometryBatchResult. We shorten the lifetime of the WKBs from 'static
        // to '_, so that the caller can use the WKBs without worrying about the lifetime.
        &self.wkbs
    }

    pub fn in_mem_size(&self) -> usize {
        let distance_in_mem_size = match &self.distance {
            Some(ColumnarValue::Array(array)) => array.get_array_memory_size(),
            _ => 8,
        };

        // Note: this is not an accurate, because wkbs has inner Vecs. However, the size of inner vecs
        // should be small, so the inaccuracy does not matter too much.
        let wkb_vec_size = self.wkbs.allocated_size();

        // We do not take wkb_array into consideration, since it is a reference to some of the
        // columns of the geometry_array.
        self.geometry_array.get_array_memory_size()
            + self.rects.allocated_size()
            + distance_in_mem_size
            + wkb_vec_size
    }
}

/// Evaluator for a relation predicate.
#[derive(Debug)]
struct RelationOperandEvaluator {
    inner: RelationPredicate,
    _options: SpatialJoinOptions,
}

impl RelationOperandEvaluator {
    pub fn new(inner: RelationPredicate, options: SpatialJoinOptions) -> Self {
        Self {
            inner,
            _options: options,
        }
    }
}

/// Evaluator for a distance predicate.
#[derive(Debug)]
struct DistanceOperandEvaluator {
    inner: DistancePredicate,
    _options: SpatialJoinOptions,
}

impl DistanceOperandEvaluator {
    pub fn new(inner: DistancePredicate, options: SpatialJoinOptions) -> Self {
        Self {
            inner,
            _options: options,
        }
    }
}

fn evaluate_with_rects(
    batch: &RecordBatch,
    geom_expr: &Arc<dyn PhysicalExpr>,
) -> Result<EvaluatedGeometryArray> {
    let geometry_columnar_value = geom_expr.evaluate(batch)?;
    let num_rows = batch.num_rows();
    let geometry_array = geometry_columnar_value.to_array(num_rows)?;
    EvaluatedGeometryArray::try_new(geometry_array)
}

impl DistanceOperandEvaluator {
    fn evaluate_with_rects(
        &self,
        batch: &RecordBatch,
        geom_expr: &Arc<dyn PhysicalExpr>,
        side: JoinSide,
    ) -> Result<EvaluatedGeometryArray> {
        let mut result = evaluate_with_rects(batch, geom_expr)?;

        let should_expand = match side {
            JoinSide::Left => self.inner.distance_side == JoinSide::Left,
            JoinSide::Right => self.inner.distance_side != JoinSide::Left,
            JoinSide::None => unreachable!(),
        };

        if !should_expand {
            return Ok(result);
        }

        // Expand the vec by distance
        let distance_columnar_value = self.inner.distance.evaluate(batch)?;
        match &distance_columnar_value {
            ColumnarValue::Scalar(ScalarValue::Float64(Some(distance))) => {
                result.rects.iter_mut().for_each(|(_, rect)| {
                    expand_rect_in_place(rect, *distance);
                });
            }
            ColumnarValue::Scalar(ScalarValue::Float64(None)) => {
                // Distance expression evaluates to NULL, the resulting distance should be NULL as well.
                result.rects.clear();
            }
            ColumnarValue::Array(array) => {
                if let Some(array) = array.as_any().downcast_ref::<Float64Array>() {
                    array
                        .iter()
                        .zip(result.rects.iter_mut())
                        .for_each(|(distance, (_, rect))| {
                            if let Some(distance) = distance {
                                expand_rect_in_place(rect, distance);
                            }
                        });
                } else {
                    return Err(DataFusionError::Internal(
                        "Distance columnar value is not a Float64Array".to_string(),
                    ));
                }
            }
            _ => {
                return Err(DataFusionError::Internal(
                    "Distance columnar value is not a Float64".to_string(),
                ));
            }
        }

        result.distance = Some(distance_columnar_value);
        Ok(result)
    }
}

fn expand_rect_in_place(rect: &mut Rect, distance: f64) {
    let mut min = rect.min();
    let mut max = rect.max();
    min.x -= distance;
    min.y -= distance;
    max.x += distance;
    max.y += distance;
    rect.set_min(min);
    rect.set_max(max);
}

impl OperandEvaluator for DistanceOperandEvaluator {
    fn evaluate_build(&self, batch: &RecordBatch) -> Result<EvaluatedGeometryArray> {
        let geom_expr = self.build_side_expr()?;
        self.evaluate_with_rects(batch, &geom_expr, JoinSide::Left)
    }

    fn evaluate_probe(&self, batch: &RecordBatch) -> Result<EvaluatedGeometryArray> {
        let geom_expr = self.probe_side_expr()?;
        self.evaluate_with_rects(batch, &geom_expr, JoinSide::Right)
    }

    fn build_side_expr(&self) -> Result<Arc<dyn PhysicalExpr>> {
        Ok(Arc::clone(&self.inner.left))
    }

    fn probe_side_expr(&self) -> Result<Arc<dyn PhysicalExpr>> {
        Ok(Arc::clone(&self.inner.right))
    }

    fn resolve_distance(
        &self,
        build_distance: &Option<ColumnarValue>,
        probe_distance: &Option<ColumnarValue>,
        row_idx: usize,
    ) -> Result<Option<f64>> {
        let distance = match self.inner.distance_side {
            JoinSide::Left => build_distance,
            JoinSide::Right | JoinSide::None => probe_distance,
        };

        let Some(distance) = distance else {
            return Ok(None);
        };

        match distance {
            ColumnarValue::Scalar(ScalarValue::Float64(Some(distance))) => Ok(Some(*distance)),
            ColumnarValue::Scalar(ScalarValue::Float64(None)) => Ok(None),
            ColumnarValue::Array(array) => {
                let array = array.as_any().downcast_ref::<Float64Array>().ok_or(
                    DataFusionError::Internal(
                        "Distance columnar value is not a Float64Array".to_string(),
                    ),
                )?;
                let distance = array.value(row_idx);
                Ok(Some(distance))
            }
            _ => Err(DataFusionError::Internal(
                "Distance columnar value is not a Float64".to_string(),
            )),
        }
    }
}

impl OperandEvaluator for RelationOperandEvaluator {
    fn build_side_expr(&self) -> Result<Arc<dyn PhysicalExpr>> {
        Ok(Arc::clone(&self.inner.left))
    }

    fn probe_side_expr(&self) -> Result<Arc<dyn PhysicalExpr>> {
        Ok(Arc::clone(&self.inner.right))
    }
}
