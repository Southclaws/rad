package tracker

import (
	"bytes"
	"testing"

	"github.com/Southclaws/rad/clients/go/protocol"
)

func TestBinaryIdentifierRecordDecoding(t *testing.T) {
	record := protocol.Record{
		"raw":  "AP8=",
		"uuid": "00000000-0000-0000-0000-000000000001",
		"ulid": "00000000000000000000000001",
		"xid":  "00000000000000000001",
	}

	if got := recBytes(record, "raw"); !bytes.Equal(got, []byte{0, 0xff}) {
		t.Fatalf("raw bytes decoded as %x", got)
	}
	if got := recUUID(record, "uuid"); got[15] != 1 {
		t.Fatalf("UUID decoded as %x", got)
	}
	if got := recULID(record, "ulid"); got[15] != 1 {
		t.Fatalf("ULID decoded as %x", got)
	}
	if got := recXID(record, "xid"); got[11] != 1 {
		t.Fatalf("XID decoded as %x", got)
	}
}
