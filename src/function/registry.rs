// Copyright 2024 KipData/KiteSQL
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::errors::DatabaseError;
use crate::expression::function::scala::ScalarFunctionImpl;
use crate::expression::function::table::{
    ArcTableFunctionImpl, TableFunctionCatalog, TableFunctionImpl,
};
use crate::expression::function::FunctionSummary;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Default)]
pub struct FunctionRegistry {
    scalar_functions: HashMap<FunctionSummary, Arc<dyn ScalarFunctionImpl>>,
    pub(crate) table_functions: HashMap<FunctionSummary, Arc<TableFunctionCatalog>>,
}

impl FunctionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_scalar(
        &mut self,
        function: Arc<dyn ScalarFunctionImpl>,
    ) -> Result<(), DatabaseError> {
        let summary = function.summary().clone();
        if self.scalar_functions.contains_key(&summary) {
            return Err(DatabaseError::FunctionAlreadyExists(
                summary.name.to_string(),
            ));
        }
        self.scalar_functions.insert(summary, function);
        Ok(())
    }

    pub fn register_table(
        &mut self,
        catalog: Arc<TableFunctionCatalog>,
    ) -> Result<(), DatabaseError> {
        let summary = catalog.inner.summary().clone();
        if self.table_functions.contains_key(&summary) {
            return Err(DatabaseError::FunctionAlreadyExists(
                summary.name.to_string(),
            ));
        }
        self.table_functions.insert(summary, catalog);
        Ok(())
    }

    pub fn get_scalar(&self, summary: &FunctionSummary) -> Option<&Arc<dyn ScalarFunctionImpl>> {
        self.scalar_functions.get(summary)
    }

    pub fn get_table(&self, summary: &FunctionSummary) -> Option<&Arc<TableFunctionCatalog>> {
        self.table_functions.get(summary)
    }

    pub fn scalar_functions(&self) -> &HashMap<FunctionSummary, Arc<dyn ScalarFunctionImpl>> {
        &self.scalar_functions
    }

    pub fn table_functions(&self) -> &HashMap<FunctionSummary, Arc<TableFunctionCatalog>> {
        &self.table_functions
    }

    pub(crate) fn load_scalar(&mut self, function: Arc<dyn ScalarFunctionImpl>) {
        let summary = function.summary().clone();
        self.scalar_functions.insert(summary, function);
    }

    pub(crate) fn load_table(&mut self, catalog: Arc<TableFunctionCatalog>) {
        let summary = catalog.inner.summary().clone();
        self.table_functions.insert(summary, catalog);
    }
}
