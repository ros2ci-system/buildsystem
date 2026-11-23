// Copyright 2026 The ros2ci authors
// SPDX-License-Identifier: Apache-2.0

//! Tests against a real PostgreSQL server.
//!
//! They run only when `CI_POSTGRES_URL` names a server, for example
//! `postgres://postgres:password@127.0.0.1:5432/postgres`. Without it they
//! pass without doing anything, and say so. Every table they create is
//! temporary, so they leave nothing behind.

use ci_postgres::{Config, Connection, Error, Param};

fn config() -> Option<Config> {
    match std::env::var("CI_POSTGRES_URL") {
        Ok(url) if !url.is_empty() => Some(Config::parse(&url).unwrap()),
        _ => {
            eprintln!("skipped: CI_POSTGRES_URL is not set");
            None
        }
    }
}

fn connect() -> Option<Connection> {
    Some(Connection::connect(&config()?).unwrap())
}

#[test]
fn connects_and_selects() {
    let Some(mut db) = connect() else { return };
    let rows = db
        .query("SELECT 1 AS one, 'two' AS two, NULL AS nothing", &[])
        .unwrap();
    assert_eq!(rows.columns, ["one", "two", "nothing"]);
    assert_eq!(rows.len(), 1);
    let row = rows.first().unwrap();
    assert_eq!(row.int(0).unwrap(), 1);
    assert_eq!(row.text(1).unwrap(), "two");
    assert_eq!(row.get(2), None);
    assert_eq!(rows.affected, 1);
}

#[test]
fn wrong_password_is_refused() {
    let Some(mut config) = config() else { return };
    config.password.push_str("-wrong");
    match Connection::connect(&config) {
        Err(Error::Server(e)) => assert_eq!(e.code, "28P01", "{e:?}"),
        Err(other) => panic!("expected a password error from the server, got {other}"),
        Ok(_) => {
            // A server set to trust local connections accepts anything,
            // and then there is nothing to test.
            eprintln!("skipped: the server does not check passwords");
        }
    }
}

#[test]
fn unknown_database_is_reported() {
    let Some(mut config) = config() else { return };
    config.database = "ros2ci_no_such_database".to_string();
    match Connection::connect(&config) {
        Err(Error::Server(e)) => assert_eq!(e.code, "3D000", "{e:?}"),
        other => panic!(
            "expected an unknown-database error, got {:?}",
            other.map(|_| ())
        ),
    }
}

#[test]
fn parameters_of_each_kind() {
    let Some(mut db) = connect() else { return };
    let rows = db
        .query(
            "SELECT $1::text, $2::bigint + 1, NOT $3::boolean, $4::text IS NULL, $5::text",
            &[
                Param::Text("héllo wörld 😀"),
                Param::Int(41),
                Param::Bool(false),
                Param::Null,
                Param::Text(""),
            ],
        )
        .unwrap();
    let row = rows.first().unwrap();
    assert_eq!(row.text(0).unwrap(), "héllo wörld 😀");
    assert_eq!(row.int(1).unwrap(), 42);
    assert!(row.bool(2).unwrap());
    assert!(row.bool(3).unwrap());
    assert_eq!(row.get(4), Some(""));

    let rows = db
        .query(
            "SELECT $1::bigint, $2::bigint",
            &[Param::Int(i64::MAX), Param::Int(i64::MIN)],
        )
        .unwrap();
    assert_eq!(rows.first().unwrap().int(0).unwrap(), i64::MAX);
    assert_eq!(rows.first().unwrap().int(1).unwrap(), i64::MIN);
}

#[test]
fn parameters_are_data_not_sql() {
    let Some(mut db) = connect() else { return };
    db.batch("CREATE TEMP TABLE t_inject (name text)").unwrap();
    let hostile = "x'); DROP TABLE t_inject; --";
    db.execute("INSERT INTO t_inject VALUES ($1)", &[hostile.into()])
        .unwrap();
    let rows = db.query("SELECT name FROM t_inject", &[]).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows.first().unwrap().text(0).unwrap(), hostile);
}

#[test]
fn bad_parameters_are_refused_before_sending() {
    let Some(mut db) = connect() else { return };
    assert!(matches!(
        db.query("SELECT $1::text", &[Param::Text("a\0b")]),
        Err(Error::Config(_))
    ));
    assert!(matches!(db.query("SELECT 1\0", &[]), Err(Error::Config(_))));
    // The connection is still in step.
    assert_eq!(
        db.query("SELECT 7", &[])
            .unwrap()
            .first()
            .unwrap()
            .int(0)
            .unwrap(),
        7
    );
}

#[test]
fn row_counts() {
    let Some(mut db) = connect() else { return };
    db.batch("CREATE TEMP TABLE t_counts (id bigint PRIMARY KEY, state text NOT NULL)")
        .unwrap();
    let inserted = db
        .execute(
            "INSERT INTO t_counts SELECT g, 'queued' FROM generate_series(1, 5) g",
            &[],
        )
        .unwrap();
    assert_eq!(inserted, 5);
    let updated = db
        .execute(
            "UPDATE t_counts SET state = $1 WHERE id <= $2",
            &["running".into(), 3i64.into()],
        )
        .unwrap();
    assert_eq!(updated, 3);
    let returned = db
        .query(
            "DELETE FROM t_counts WHERE state = $1 RETURNING id",
            &["running".into()],
        )
        .unwrap();
    assert_eq!(returned.affected, 3);
    let mut ids: Vec<i64> = returned.iter().map(|r| r.int(0).unwrap()).collect();
    ids.sort();
    assert_eq!(ids, [1, 2, 3]);
    assert_eq!(
        db.execute("DELETE FROM t_counts WHERE id = 99", &[])
            .unwrap(),
        0
    );
}

#[test]
fn server_errors_leave_the_connection_usable() {
    let Some(mut db) = connect() else { return };
    db.batch("CREATE TEMP TABLE t_unique (id bigint PRIMARY KEY)")
        .unwrap();
    db.execute("INSERT INTO t_unique VALUES (1)", &[]).unwrap();

    let duplicate = db
        .execute("INSERT INTO t_unique VALUES (1)", &[])
        .unwrap_err();
    assert_eq!(duplicate.sqlstate(), Some("23505"), "{duplicate}");

    let syntax = db.query("SELEKT 1", &[]).unwrap_err();
    assert_eq!(syntax.sqlstate(), Some("42601"), "{syntax}");

    let missing = db
        .query("SELECT * FROM ros2ci_no_such_table", &[])
        .unwrap_err();
    assert_eq!(missing.sqlstate(), Some("42P01"), "{missing}");

    let wrong_count = db.query("SELECT $1::int", &[]).unwrap_err();
    assert!(wrong_count.sqlstate().is_some(), "{wrong_count}");

    assert!(db.is_usable());
    assert_eq!(
        db.query("SELECT count(*) FROM t_unique", &[])
            .unwrap()
            .first()
            .unwrap()
            .int(0)
            .unwrap(),
        1
    );
}

#[test]
fn transactions_commit_and_roll_back() {
    let Some(mut db) = connect() else { return };
    db.batch("CREATE TEMP TABLE t_tx (id bigint PRIMARY KEY)")
        .unwrap();

    db.transaction(|db| db.execute("INSERT INTO t_tx VALUES (1)", &[]).map(|_| ()))
        .unwrap();

    let failed: Result<(), Error> = db.transaction(|db| {
        db.execute("INSERT INTO t_tx VALUES (2)", &[])?;
        // A duplicate key fails the transaction.
        db.execute("INSERT INTO t_tx VALUES (1)", &[])?;
        Ok(())
    });
    assert_eq!(failed.unwrap_err().sqlstate(), Some("23505"));

    let rows = db.query("SELECT id FROM t_tx ORDER BY id", &[]).unwrap();
    let ids: Vec<i64> = rows.iter().map(|r| r.int(0).unwrap()).collect();
    assert_eq!(ids, [1], "the failed transaction must leave nothing behind");
}

#[test]
fn batch_runs_several_statements() {
    let Some(mut db) = connect() else { return };
    db.batch(
        "CREATE TEMP TABLE t_batch (id bigint);
         INSERT INTO t_batch VALUES (1), (2);
         INSERT INTO t_batch VALUES (3);",
    )
    .unwrap();
    assert_eq!(
        db.query("SELECT count(*) FROM t_batch", &[])
            .unwrap()
            .first()
            .unwrap()
            .int(0)
            .unwrap(),
        3
    );

    // An error part-way through is reported, and the connection survives.
    let err = db
        .batch("INSERT INTO t_batch VALUES (4); SELEKT 1;")
        .unwrap_err();
    assert_eq!(err.sqlstate(), Some("42601"));
    assert!(db.is_usable());
    db.batch("").unwrap();
}

#[test]
fn large_values_and_many_rows() {
    let Some(mut db) = connect() else { return };
    let big = "0123456789abcdef".repeat(128 * 1024); // 2 MiB
    let rows = db
        .query("SELECT $1::text, length($1::text)", &[Param::Text(&big)])
        .unwrap();
    assert_eq!(rows.first().unwrap().text(0).unwrap(), big);
    assert_eq!(rows.first().unwrap().int(1).unwrap(), big.len() as i64);

    let rows = db
        .query(
            "SELECT g, g::text || '-row' FROM generate_series(1, 50000) g",
            &[],
        )
        .unwrap();
    assert_eq!(rows.len(), 50000);
    assert_eq!(rows.affected, 50000);
    assert_eq!(rows.rows[49999].int(0).unwrap(), 50000);
    assert_eq!(rows.rows[49999].text(1).unwrap(), "50000-row");
}

#[test]
fn common_column_types_as_text() {
    let Some(mut db) = connect() else { return };
    let rows = db
        .query(
            "SELECT true, 1.5::numeric, 'x'::bytea, '2026-11-23 10:00:00+00'::timestamptz AT TIME ZONE 'UTC', \
             ARRAY[1,2,3], '{\"a\": 1}'::jsonb",
            &[],
        )
        .unwrap();
    let row = rows.first().unwrap();
    assert_eq!(row.get(0), Some("t"));
    assert_eq!(row.get(1), Some("1.5"));
    assert_eq!(row.get(2), Some("\\x78"));
    assert_eq!(row.get(3), Some("2026-11-23 10:00:00"));
    assert_eq!(row.get(4), Some("{1,2,3}"));
    assert_eq!(row.get(5), Some("{\"a\": 1}"));
}

/// The two database features the job queue depends on: claiming a row
/// that no other session has locked, and a lock that only one session can
/// hold.
#[test]
fn queue_primitives_across_two_sessions() {
    let Some(mut a) = connect() else { return };
    let mut b = connect().unwrap();
    let table = format!("ros2ci_test_queue_{}", std::process::id());
    a.batch(&format!(
        "DROP TABLE IF EXISTS {table}; CREATE TABLE {table} (id bigint PRIMARY KEY); \
         INSERT INTO {table} VALUES (1), (2);"
    ))
    .unwrap();
    let claim = format!("SELECT id FROM {table} ORDER BY id LIMIT 1 FOR UPDATE SKIP LOCKED");

    a.execute("BEGIN", &[]).unwrap();
    b.execute("BEGIN", &[]).unwrap();
    let first = a
        .query(&claim, &[])
        .unwrap()
        .first()
        .unwrap()
        .int(0)
        .unwrap();
    let second = b
        .query(&claim, &[])
        .unwrap()
        .first()
        .unwrap()
        .int(0)
        .unwrap();
    assert_eq!(
        (first, second),
        (1, 2),
        "two sessions must claim different rows"
    );
    a.execute("COMMIT", &[]).unwrap();
    b.execute("COMMIT", &[]).unwrap();
    a.batch(&format!("DROP TABLE {table}")).unwrap();

    let key = 7_021_126 + i64::from(std::process::id());
    let lock = "SELECT pg_try_advisory_lock($1)";
    assert!(a
        .query(lock, &[key.into()])
        .unwrap()
        .first()
        .unwrap()
        .bool(0)
        .unwrap());
    assert!(!b
        .query(lock, &[key.into()])
        .unwrap()
        .first()
        .unwrap()
        .bool(0)
        .unwrap());
    // The lock belongs to the session, and goes when the session ends.
    drop(a);
    let mut acquired = false;
    for _ in 0..50 {
        if b.query(lock, &[key.into()])
            .unwrap()
            .first()
            .unwrap()
            .bool(0)
            .unwrap()
        {
            acquired = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(
        acquired,
        "the lock should be free once its session has ended"
    );
}
