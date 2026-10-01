# sqlite (vendored amalgamation)

`sqlite3.c` + `sqlite3.h` are the official SQLite amalgamation (3.45.1,
public domain), compiled straight into the binary when the `db` feature is
enabled — no system sqlite needed.

To bump the version:

```bash
curl -fL -o amalg.zip https://sqlite.org/2024/sqlite-amalgamation-3450100.zip
unzip amalg.zip
cp sqlite-amalgamation-*/sqlite3.{c,h} .
rm -rf amalg.zip sqlite-amalgamation-*
```
