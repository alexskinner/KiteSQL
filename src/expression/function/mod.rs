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

use crate::types::LogicalType;
use kite_sql_serde_macros::ReferenceSerialization;
use std::sync::Arc;

pub mod scala;
pub mod table;

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, ReferenceSerialization, Default)]
pub enum PersistencePolicy {
    Transient,
    #[default]
    Durable,
}

#[derive(Debug, Eq, PartialEq, Hash, Clone, ReferenceSerialization)]
pub struct FunctionSummary {
    pub name: Arc<str>,
    pub arg_types: Vec<LogicalType>,
    pub persistence: PersistencePolicy,
    /// If true, the function accepts any argument types and any argument count that matches
    /// the arg_types length. The resolver will skip type checking and let the function
    /// implementation handle type conversions.
    pub accepts_any_args: bool,
}
