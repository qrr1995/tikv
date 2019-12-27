// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

use crate::errors::Result;
use crate::range::Range;
use crate::CFHandleExt;

pub trait TablePropertiesExt: CFHandleExt {
    type TablePropertiesCollection: TablePropertiesCollection;

    fn get_properties_of_tables_in_range(
        &self,
        cf: &Self::CFHandle,
        ranges: &[Range],
    ) -> Result<Self::TablePropertiesCollection>;

    fn get_range_properties_cf(
        &self,
        cfname: &str,
        start_key: &[u8],
        end_key: &[u8],
    ) -> Result<Self::TablePropertiesCollection> {
        let cf = self.cf_handle(cfname)?;
        let range = Range::new(start_key, end_key);
        Ok(self.get_properties_of_tables_in_range(cf, &[range])?)
    }
}

pub trait TablePropertiesCollection {}
