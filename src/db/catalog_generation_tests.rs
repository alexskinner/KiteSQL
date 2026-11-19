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

//! Catalog generations are reference counted: these tests count the live ones.

use super::*;
use crate::types::LogicalType;
use tempfile::TempDir;

/// The newest generation plus the placeholder parked for `&mut` swaps.
const BASELINE: usize = 2;

fn exec<S: Storage>(session: &mut Session<'_, S>, sql: &str) -> Result<(), DatabaseError> {
    session.run(sql)?.done()
}

#[test]
fn session_ddl_commits_keep_live_catalogs_bounded() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let db = DataBaseBuilder::path(dir.path()).build_lmdb()?;
    let live = db.state.live_catalogs.clone();
    assert_eq!(live.count(), BASELINE);

    let mut last_generation = db.state.catalog().generation();
    let mut max_live = 0;
    for i in 0..2000 {
        let mut session = db.session();
        exec(&mut session, "BEGIN")?;
        if i % 2 == 0 {
            exec(
                &mut session,
                &format!("CREATE TABLE t{i} (id INT PRIMARY KEY)"),
            )?;
        } else {
            exec(&mut session, &format!("DROP TABLE t{}", i - 1))?;
        }
        exec(&mut session, "COMMIT")?;
        if i % 100 == 0 {
            // Autocommit statements pin a generation only while their iterator lives.
            exec(&mut session, &format!("SELECT * FROM t{i}"))?;
        }
        drop(session);

        let generation = db.state.catalog().generation();
        assert!(
            generation > last_generation,
            "generations must only increase"
        );
        last_generation = generation;
        max_live = max_live.max(live.count());
    }
    assert_eq!(live.count(), BASELINE);
    assert!(max_live <= BASELINE, "max live generations {max_live}");
    Ok(())
}

#[test]
fn pinned_generation_lives_until_its_transaction_ends() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let mut db = DataBaseBuilder::path(dir.path()).build_lmdb()?;
    db.ddl("CREATE TABLE t (id INT PRIMARY KEY)")?;
    let db = db;
    let live = db.state.live_catalogs.clone();

    let tx = db.new_transaction(true)?;
    let iter = db.run("SELECT * FROM t")?;
    for i in 0..50 {
        let mut session = db.session();
        exec(&mut session, "BEGIN")?;
        exec(
            &mut session,
            &format!("CREATE TABLE u{i} (id INT PRIMARY KEY)"),
        )?;
        exec(&mut session, "COMMIT")?;
    }
    // The transaction and the iterator share the one generation they pinned.
    assert_eq!(live.count(), BASELINE + 1);
    iter.done()?;
    assert_eq!(live.count(), BASELINE + 1);
    tx.commit()?;
    assert_eq!(live.count(), BASELINE);
    Ok(())
}

#[test]
fn mut_ddl_and_programs_do_not_accumulate_generations() -> Result<(), DatabaseError> {
    let dir = TempDir::new().unwrap();
    let mut db = DataBaseBuilder::path(dir.path()).build_lmdb()?;
    let live = db.state.live_catalogs.clone();
    for i in 0..300 {
        db.ddl(format!("CREATE TABLE m{i} (id INT PRIMARY KEY)"))?;
    }
    assert_eq!(live.count(), BASELINE);

    // Programs keep the generation they were prepared against for the `&Database` borrow:
    // one retained generation per distinct generation, released by the next `&mut` call.
    {
        let mut programs = Vec::new();
        for i in 0..20 {
            programs.push(db.program_sql(
                "SELECT * FROM m0 WHERE id = $1",
                &[(1, LogicalType::Integer)],
            )?);
            programs.push(db.program_sql(
                "SELECT * FROM m1 WHERE id = $1",
                &[(1, LogicalType::Integer)],
            )?);
            let mut session = db.session();
            exec(&mut session, "BEGIN")?;
            exec(
                &mut session,
                &format!("CREATE TABLE p{i} (id INT PRIMARY KEY)"),
            )?;
            exec(&mut session, "COMMIT")?;
        }
        assert_eq!(live.count(), BASELINE + 20);
    }
    db.ddl("CREATE TABLE after_programs (id INT PRIMARY KEY)")?;
    assert_eq!(live.count(), BASELINE);
    Ok(())
}
