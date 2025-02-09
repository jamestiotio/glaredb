import os.path
import random

import bson
import psycopg2.extensions
import psycopg2.extras
import pytest

from fixtures.glaredb import glaredb_connection, debug_path


def test_copy_to(
    glaredb_connection: psycopg2.extensions.connection,
    tmp_path_factory: pytest.TempPathFactory,
):
    with glaredb_connection.cursor() as curr:
        curr.execute("create table bson_test (amount int)")

    with glaredb_connection.cursor() as curr:
        for i in range(10):
            curr.execute("insert into bson_test values (%s)", str(i))

    with glaredb_connection.cursor() as curr:
        curr.execute("select count(*) from bson_test;")
        res = curr.fetchone()

        assert res[0] == 10

    output_path = tmp_path_factory.mktemp("output").joinpath("copy_output.bson")

    assert not os.path.exists(output_path)

    with glaredb_connection.cursor() as curr:
        print(output_path)
        curr.execute(f"COPY( SELECT * FROM bson_test ) TO '{output_path}'")

    assert os.path.exists(output_path)

    with open(output_path, "rb") as f:
        for idx, doc in enumerate(bson.decode_file_iter(f)):
            print(doc)

            assert len(doc) == 1
            assert "amount" in doc
            assert doc["amount"] == idx


def test_read_bson(
    glaredb_connection: psycopg2.extensions.connection,
    tmp_path_factory: pytest.TempPathFactory,
):
    beatles = ["john", "paul", "george", "ringo"]

    tmp_dir = tmp_path_factory.mktemp(basename="read-bson-beatles-", numbered=True)
    data_path = tmp_dir.joinpath("beatles.100.bson")

    with open(data_path, "wb") as f:
        for i in range(100):
            beatle_id = random.randrange(0, len(beatles))
            f.write(
                bson.encode(
                    {
                        "_id": bson.objectid.ObjectId(),
                        "beatle_idx": beatle_id + 1,
                        "beatle_name": beatles[beatle_id],
                        "case": i + 1,
                        "rand": random.random(),
                    }
                )
            )

    with glaredb_connection.cursor() as curr:
        curr.execute(
            f"create external table bson_beatles from bson options ( location='{data_path}', file_type='bson')"
        )

    for from_clause in ["bson_beatles", f"read_bson('{data_path}')"]:
        with glaredb_connection.cursor() as curr:
            curr.execute(f"select count(*) from {from_clause}")
            r = curr.fetchone()
            assert r[0] == 100

        with glaredb_connection.cursor(cursor_factory=psycopg2.extras.RealDictCursor) as curr:
            curr.execute(f"select * from {from_clause}")
            rows = curr.fetchall()
            assert len(rows) == 100
            for row in rows:
                assert len(row) == 5
                assert row["beatle_name"] in beatles
                assert beatles.index(row["beatle_name"]) == row["beatle_idx"] - 1
