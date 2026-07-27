package sql

import (
	"strings"
	"testing"
)

func TestTransactionControlIsTransportMetadata(t *testing.T) {
	schema, err := NewSchema(nil)
	if err != nil {
		t.Fatal(err)
	}
	for _, tc := range []struct {
		sql  string
		want TransactionControl
		tag  string
	}{
		{sql: "BEGIN", want: TransactionBegin, tag: "BEGIN"},
		{sql: "START TRANSACTION", want: TransactionBegin, tag: "BEGIN"},
		{sql: "COMMIT", want: TransactionCommit, tag: "COMMIT"},
		{sql: "ROLLBACK", want: TransactionRollback, tag: "ROLLBACK"},
	} {
		t.Run(tc.sql, func(t *testing.T) {
			statements, err := Parse(tc.sql)
			if err != nil {
				t.Fatal(err)
			}
			prepared, err := Prepare(schema, statements[0])
			if err != nil {
				t.Fatal(err)
			}
			compiled, err := prepared.Compile(nil)
			if err != nil {
				t.Fatal(err)
			}
			if compiled.Transaction != tc.want || compiled.Tag != tc.tag || compiled.Program != nil {
				t.Fatalf("compiled = %#v, want control %d tag %q and no PIR program", compiled, tc.want, tc.tag)
			}
		})
	}
}

func TestSavepointsRejectedUntilStorageSupportsThem(t *testing.T) {
	schema, err := NewSchema(nil)
	if err != nil {
		t.Fatal(err)
	}
	statements, err := Parse("SAVEPOINT nested")
	if err != nil {
		t.Fatal(err)
	}
	_, err = Prepare(schema, statements[0])
	if err == nil || !strings.Contains(err.Error(), "savepoints") {
		t.Fatalf("error = %v, want explicit savepoint rejection", err)
	}
}

func TestTransactionOptionsDoNotPromiseUnsupportedSemantics(t *testing.T) {
	schema, err := NewSchema(nil)
	if err != nil {
		t.Fatal(err)
	}
	for _, tc := range []struct {
		sql     string
		wantErr string
	}{
		{sql: "BEGIN ISOLATION LEVEL SERIALIZABLE"},
		{sql: "START TRANSACTION ISOLATION LEVEL SERIALIZABLE, READ WRITE, NOT DEFERRABLE"},
		{sql: "BEGIN ISOLATION LEVEL READ COMMITTED", wantErr: "currently provides serializable"},
		{sql: "BEGIN ISOLATION LEVEL REPEATABLE READ", wantErr: "currently provides serializable"},
		{sql: "BEGIN READ ONLY", wantErr: "read-only transactions"},
		{sql: "BEGIN DEFERRABLE", wantErr: "deferrable transactions"},
		{sql: "COMMIT AND CHAIN", wantErr: "AND CHAIN"},
		{sql: "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE"},
		{sql: "SET TRANSACTION ISOLATION LEVEL READ COMMITTED", wantErr: "currently provides serializable"},
	} {
		t.Run(tc.sql, func(t *testing.T) {
			statements, err := Parse(tc.sql)
			if err != nil {
				t.Fatal(err)
			}
			_, err = Prepare(schema, statements[0])
			if tc.wantErr == "" && err != nil {
				t.Fatal(err)
			}
			if tc.wantErr != "" && (err == nil || !strings.Contains(err.Error(), tc.wantErr)) {
				t.Fatalf("statement = %#v, error = %v, want %q", statements[0], err, tc.wantErr)
			}
		})
	}
}
