use adbc_core::{
    error::{Error, Result, Status},
    options::{OptionConnection, OptionDatabase, OptionValue},
    Database, Optionable,
};

use crate::{connection::SedonaConnection, err_unrecognized_option};

pub struct SedonaDatabase {}

impl Optionable for SedonaDatabase {
    type Option = OptionDatabase;

    fn set_option(&mut self, key: Self::Option, _value: OptionValue) -> Result<()> {
        err_unrecognized_option!(key)
    }

    fn get_option_string(&self, key: Self::Option) -> Result<String> {
        err_unrecognized_option!(key)
    }

    fn get_option_bytes(&self, key: Self::Option) -> Result<Vec<u8>> {
        err_unrecognized_option!(key)
    }

    fn get_option_int(&self, key: Self::Option) -> Result<i64> {
        err_unrecognized_option!(key)
    }

    fn get_option_double(&self, key: Self::Option) -> Result<f64> {
        err_unrecognized_option!(key)
    }
}

impl Database for SedonaDatabase {
    type ConnectionType = SedonaConnection;

    fn new_connection(&mut self) -> Result<SedonaConnection> {
        self.new_connection_with_opts([])
    }

    fn new_connection_with_opts(
        &mut self,
        opts: impl IntoIterator<Item = (OptionConnection, OptionValue)>,
    ) -> Result<SedonaConnection> {
        SedonaConnection::try_new(opts)
    }
}
