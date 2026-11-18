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
use crate::types::LogicalType;
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

    pub fn resolve_scalar(
        &self,
        name: &str,
        arg_types: &[LogicalType],
    ) -> Result<Arc<dyn ScalarFunctionImpl>, DatabaseError> {
        let candidates: Vec<_> = self
            .scalar_functions
            .iter()
            .filter(|(summary, _)| summary.name.as_ref() == name)
            .filter(|(summary, _)| summary.arg_types.len() == arg_types.len())
            .collect();

        if candidates.is_empty() {
            return Err(DatabaseError::function_not_found(name));
        }

        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
        enum MatchQuality {
            ExactMatch,
            ImplicitCast,
            NullLiteral,
        }

        let mut scored_candidates = Vec::new();

        for (summary, function) in candidates {
            let mut overall_quality = MatchQuality::ExactMatch;
            let mut compatible = true;

            for (provided, expected) in arg_types.iter().zip(summary.arg_types.iter()) {
                if provided == expected {
                    continue;
                } else if provided == &LogicalType::SqlNull {
                    overall_quality = overall_quality.max(MatchQuality::NullLiteral);
                } else if LogicalType::can_implicit_cast(provided, expected) {
                    overall_quality = overall_quality.max(MatchQuality::ImplicitCast);
                } else {
                    compatible = false;
                    break;
                }
            }

            if compatible {
                scored_candidates.push((overall_quality, function.clone()));
            }
        }

        if scored_candidates.is_empty() {
            return Err(DatabaseError::function_not_found(format!(
                "{} with argument types {:?}",
                name, arg_types
            )));
        }

        scored_candidates.sort_by_key(|(quality, _)| *quality);

        Ok(scored_candidates[0].1.clone())
    }
}
