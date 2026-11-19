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

#[cfg(all(not(target_arch = "wasm32"), feature = "lmdb"))]
mod tests {
    use kite_sql::db::DataBaseBuilder;
    use tempfile::TempDir;

    #[test]
    fn test_update_with_self_join_alias() {
        let temp_dir = TempDir::new().unwrap();
        let mut db = DataBaseBuilder::path(temp_dir.path()).build_lmdb().unwrap();

        db.ddl("CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER, y INTEGER)")
            .unwrap();
        db.run("INSERT INTO t VALUES (1, 10, 20), (2, 30, 40)")
            .unwrap()
            .done()
            .unwrap();

        db.run("UPDATE t latest SET latest.x = 99 WHERE latest.id = 1")
            .unwrap()
            .done()
            .unwrap();

        let mut iter = db.run("SELECT * FROM t ORDER BY id").unwrap();
        let mut rows = Vec::new();
        while let Some(row) = iter
            .next_tuple(|_, tuple| {
                (
                    tuple[0].i32().unwrap(),
                    tuple[1].i32().unwrap(),
                    tuple[2].i32().unwrap(),
                )
            })
            .unwrap()
        {
            rows.push(row);
        }
        iter.done().unwrap();
        assert_eq!(rows, vec![(1, 99, 20), (2, 30, 40)]);
    }

    #[test]
    fn test_update_with_left_join() {
        let temp_dir = TempDir::new().unwrap();
        let mut db = DataBaseBuilder::path(temp_dir.path()).build_lmdb().unwrap();

        db.ddl("CREATE TABLE _rev_records (record_id INTEGER PRIMARY KEY, id INTEGER, _rev_number INTEGER, _rev_is_latest_draft VARCHAR(1))")
            .unwrap();
        db.run("INSERT INTO _rev_records VALUES (1, 1, 1, '0'), (2, 1, 2, '0'), (3, 2, 1, '0')")
            .unwrap()
            .done()
            .unwrap();

        let mut iter = db
            .run(
                "UPDATE `_rev_records` `latest` \
                 LEFT JOIN `_rev_records` `older` \
                 ON `older`.id = `latest`.id AND `older`._rev_number > `latest`._rev_number \
                 SET `latest`._rev_is_latest_draft = '1' \
                 WHERE `older`.id IS NULL",
            )
            .unwrap();
        let count = iter
            .next_tuple(|_, tuple| tuple[0].utf8().unwrap().parse::<i32>().unwrap())
            .unwrap()
            .unwrap();
        iter.done().unwrap();
        assert_eq!(count, 2, "Should update exactly 2 rows");

        let mut iter = db
            .run("SELECT * FROM _rev_records ORDER BY record_id")
            .unwrap();
        let mut rows = Vec::new();
        while let Some(row) = iter
            .next_tuple(|_, tuple| {
                (
                    tuple[0].i32().unwrap(),
                    tuple[1].i32().unwrap(),
                    tuple[2].i32().unwrap(),
                    tuple[3].utf8().unwrap().to_string(),
                )
            })
            .unwrap()
        {
            rows.push(row);
        }
        iter.done().unwrap();
        assert_eq!(
            rows,
            vec![
                (1, 1, 1, "0".to_string()),
                (2, 1, 2, "1".to_string()),
                (3, 2, 1, "1".to_string())
            ]
        );
    }

    #[test]
    fn test_update_with_inner_join() {
        let temp_dir = TempDir::new().unwrap();
        let mut db = DataBaseBuilder::path(temp_dir.path()).build_lmdb().unwrap();

        db.ddl("CREATE TABLE products (id INTEGER PRIMARY KEY, name VARCHAR(100), price INTEGER)")
            .unwrap();
        db.ddl("CREATE TABLE price_updates (product_id INTEGER PRIMARY KEY, new_price INTEGER)")
            .unwrap();
        db.run(
            "INSERT INTO products VALUES (1, 'apple', 100), (2, 'banana', 50), (3, 'cherry', 75)",
        )
        .unwrap()
        .done()
        .unwrap();
        db.run("INSERT INTO price_updates VALUES (1, 150), (2, 60)")
            .unwrap()
            .done()
            .unwrap();

        let mut iter = db
            .run(
                "UPDATE products p \
                 INNER JOIN price_updates pu ON p.id = pu.product_id \
                 SET p.price = pu.new_price",
            )
            .unwrap();
        let count = iter
            .next_tuple(|_, tuple| tuple[0].utf8().unwrap().parse::<i32>().unwrap())
            .unwrap()
            .unwrap();
        iter.done().unwrap();
        assert_eq!(count, 2, "Should update exactly 2 rows");

        let mut iter = db.run("SELECT * FROM products ORDER BY id").unwrap();
        let mut rows = Vec::new();
        while let Some(row) = iter
            .next_tuple(|_, tuple| {
                (
                    tuple[0].i32().unwrap(),
                    tuple[1].utf8().unwrap().to_string(),
                    tuple[2].i32().unwrap(),
                )
            })
            .unwrap()
        {
            rows.push(row);
        }
        iter.done().unwrap();
        assert_eq!(
            rows,
            vec![
                (1, "apple".to_string(), 150),
                (2, "banana".to_string(), 60),
                (3, "cherry".to_string(), 75)
            ]
        );
    }
}
