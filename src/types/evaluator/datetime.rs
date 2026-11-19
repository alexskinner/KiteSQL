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

use crate::numeric_binary_evaluator_definition;
use crate::types::evaluator::cast::{cast_fail, to_char, to_varbinary, to_varchar};
use crate::types::evaluator::DataValue;
use crate::types::CharLengthUnits;
use crate::types::LogicalType;
use chrono::{DateTime, Datelike, Timelike};

numeric_binary_evaluator_definition!(DateTime, DataValue::Date64);
crate::define_cast_evaluator!(
    date64_to_char_cast_eval {
        len: u32,
        unit: CharLengthUnits
    },
    DataValue::Date64(value) => |this| {
        to_char(
            DataValue::format_datetime(value).ok_or_else(|| {
                cast_fail(LogicalType::DateTime, LogicalType::Char(this.len, this.unit))
            })?,
            this.len,
            this.unit,
        )
    }
);
crate::define_cast_evaluator!(
    date64_to_varchar_cast_eval {
        len: Option<u32>,
        unit: CharLengthUnits
    },
    DataValue::Date64(value) => |this| {
        to_varchar(
            DataValue::format_datetime(value).ok_or_else(|| {
                cast_fail(LogicalType::DateTime, LogicalType::Varchar(this.len, this.unit))
            })?,
            this.len,
            this.unit,
        )
    }
);
crate::define_cast_evaluator!(
    date64_to_varbinary_cast_eval { len: Option<u32> },
    DataValue::Date64(value) => |this| {
        to_varbinary(
            DataValue::format_datetime(value).ok_or_else(|| {
                cast_fail(LogicalType::DateTime, LogicalType::Varbinary(this.len))
            })?.into_bytes(),
            this.len,
        )
    }
);
fn date64_number(value: i64) -> Option<i64> {
    let dt = DateTime::from_timestamp(value, 0)?.naive_utc();
    Some(
        dt.year() as i64 * 10_000_000_000
            + dt.month() as i64 * 100_000_000
            + dt.day() as i64 * 1_000_000
            + dt.hour() as i64 * 10_000
            + dt.minute() as i64 * 100
            + dt.second() as i64,
    )
}
// MySQL's numeric form of a DATETIME: YYYYMMDDhhmmss.
crate::define_cast_evaluator!(date64_to_bigint_cast_eval, DataValue::Date64(value) => {
        let number = date64_number(value)
            .ok_or_else(|| cast_fail(LogicalType::DateTime, LogicalType::Bigint))?;
        Ok(DataValue::Int64(number))
    }
);
crate::define_cast_evaluator!(date64_to_double_cast_eval, DataValue::Date64(value) => {
        let number = date64_number(value)
            .ok_or_else(|| cast_fail(LogicalType::DateTime, LogicalType::Double))?;
        Ok(DataValue::Float64(ordered_float::OrderedFloat(number as f64)))
    }
);
crate::define_cast_evaluator!(date64_to_date_cast_eval, DataValue::Date64(value) => {
        let value = DateTime::from_timestamp(value, 0)
            .ok_or_else(|| cast_fail(LogicalType::DateTime, LogicalType::Date))?
            .naive_utc()
            .date()
            .num_days_from_ce();

        Ok(DataValue::Date32(value))
    }
);
crate::define_cast_evaluator!(
    date64_to_time_cast_eval {
        precision: Option<u64>
    },
    DataValue::Date64(value) => |this| {
        let precision = this.precision.unwrap_or(0);
        let value = DateTime::from_timestamp(value, 0)
            .map(|date_time| date_time.time().num_seconds_from_midnight())
            .ok_or_else(|| {
                cast_fail(LogicalType::DateTime, LogicalType::Time(this.precision))
            })?;

        Ok(DataValue::Time32(DataValue::pack_time(value, 0, 0), precision))
    }
);
crate::define_cast_evaluator!(
    date64_to_timestamp_cast_eval {
        precision: Option<u64>,
        zone: bool
    },
    DataValue::Date64(value) => |this| {
        Ok(DataValue::Time64(value, this.precision.unwrap_or(0), this.zone))
    }
);

// GRCOV_EXCL_START
#[cfg(all(test, not(target_arch = "wasm32")))]
#[maybe_async]
mod test {
    use super::*;
    use crate::errors::DatabaseError;
    use crate::types::value::Utf8Type;
    use crate::types::CharLengthUnits;

    #[test]
    fn test_datetime_cast_evaluators() {
        let value = DataValue::Date64(
            chrono::NaiveDate::from_ymd_opt(2024, 1, 2)
                .unwrap()
                .and_hms_opt(3, 4, 5)
                .unwrap()
                .and_utc()
                .timestamp(),
        );
        assert_eq!(
            date64_to_char_cast_eval(19, CharLengthUnits::Characters, value.clone()).unwrap(),
            DataValue::Utf8 {
                value: "2024-01-02 03:04:05".into(),
                ty: Utf8Type::Fixed(19),
                unit: CharLengthUnits::Characters,
            }
        );
        assert_eq!(
            date64_to_varchar_cast_eval(Some(19), CharLengthUnits::Characters, value.clone())
                .unwrap(),
            DataValue::Utf8 {
                value: "2024-01-02 03:04:05".into(),
                ty: Utf8Type::Variable(Some(19)),
                unit: CharLengthUnits::Characters,
            }
        );
        assert_eq!(
            date64_to_date_cast_eval(value.clone()).unwrap(),
            DataValue::Date32(
                chrono::NaiveDate::from_ymd_opt(2024, 1, 2)
                    .unwrap()
                    .num_days_from_ce()
            )
        );
        assert_eq!(
            date64_to_time_cast_eval(Some(0), value.clone()).unwrap(),
            DataValue::Time32(DataValue::pack_time(3 * 3600 + 4 * 60 + 5, 0, 0), 0)
        );
        assert_eq!(
            date64_to_timestamp_cast_eval(Some(0), true, value.clone()).unwrap(),
            DataValue::Time64(
                chrono::NaiveDate::from_ymd_opt(2024, 1, 2)
                    .unwrap()
                    .and_hms_opt(3, 4, 5)
                    .unwrap()
                    .and_utc()
                    .timestamp(),
                0,
                true,
            )
        );
        assert_eq!(
            date_time_eq_binary_eval(&value, &value).unwrap(),
            DataValue::Boolean(true)
        );

        let invalid = DataValue::Date64(i64::MAX);
        assert!(matches!(
            date64_to_char_cast_eval(19, CharLengthUnits::Characters, invalid.clone()),
            Err(DatabaseError::CastFail { .. })
        ));
        assert!(matches!(
            date64_to_varchar_cast_eval(Some(19), CharLengthUnits::Characters, invalid.clone()),
            Err(DatabaseError::CastFail { .. })
        ));
        assert!(matches!(
            date64_to_date_cast_eval(invalid.clone()),
            Err(DatabaseError::CastFail { .. })
        ));
        assert!(matches!(
            date64_to_time_cast_eval(Some(0), invalid.clone()),
            Err(DatabaseError::CastFail { .. })
        ));
    }
}
// GRCOV_EXCL_STOP
