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
use crate::function::registry::FunctionRegistry;
use std::fmt::Debug;

/// A pack of related functions that can be installed together into a FunctionRegistry.
///
/// Function packs allow grouping related functions (e.g., all MySQL-dialect functions)
/// and installing them as a unit. This is useful for dialect-specific function sets
/// that should be loaded together.
///
/// # Example
///
/// ```ignore
/// use kite_sql::function::pack::FunctionPack;
/// use kite_sql::function::registry::FunctionRegistry;
///
/// #[derive(Debug)]
/// struct MyFunctions;
///
/// impl FunctionPack for MyFunctions {
///     fn name(&self) -> &str { "my_functions" }
///     
///     fn install(&self, registry: &mut FunctionRegistry) -> Result<(), DatabaseError> {
///         // Register functions here
///         Ok(())
///     }
/// }
/// ```
pub trait FunctionPack: Debug + Send + Sync {
    /// Returns the name of this function pack.
    fn name(&self) -> &str;

    /// Install all functions from this pack into the given registry.
    ///
    /// This method should call `registry.register_scalar()` or `registry.register_table()`
    /// for each function in the pack.
    fn install(&self, registry: &mut FunctionRegistry) -> Result<(), DatabaseError>;
}
