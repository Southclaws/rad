package keyenc_test

import (
	"bytes"
	"fmt"

	"github.com/Southclaws/rad/rad/engine/01_kv/keyenc"
)

// The whole point of keyenc: encoded bytes sort the way the values do, so a
// KV range scan walks rows in semantic order. Naive encodings break this
// ("10" < "2" as strings, negative floats reverse, …); these do not.
func Example_orderPreservation() {
	fmt.Println(bytes.Compare(keyenc.EncodeInt64(2), keyenc.EncodeInt64(10)) < 0)
	fmt.Println(bytes.Compare(keyenc.EncodeInt64(-5), keyenc.EncodeInt64(3)) < 0)
	fmt.Println(bytes.Compare(keyenc.EncodeFloat64(-2.5), keyenc.EncodeFloat64(-1.5)) < 0)
	fmt.Println(bytes.Compare(keyenc.EncodeString("app"), keyenc.EncodeString("apple")) < 0)
	// Output:
	// true
	// true
	// true
	// true
}

// Encodings are self-delimiting: values concatenate into tuple keys with no
// separator and decode back unambiguously.
func Example_selfDelimiting() {
	key := append(keyenc.EncodeString("eu"), keyenc.EncodeInt64(42)...)

	region, n, _ := keyenc.DecodeString(key)
	id, _, _ := keyenc.DecodeInt64(key[n:])
	fmt.Printf("%s/%d\n", region, id)
	// Output:
	// eu/42
}

// PrefixEnd computes the exclusive upper bound for "every key starting with
// this prefix" — the standard way to turn a prefix into a [start, end) scan.
func ExamplePrefixEnd() {
	prefix := []byte("/rad/data/t1/")
	end := keyenc.PrefixEnd(prefix)

	fmt.Printf("scan [%s, %s)\n", prefix, end)
	fmt.Println(bytes.Compare([]byte("/rad/data/t1/anything"), end) < 0)
	fmt.Println(bytes.Compare([]byte("/rad/data/t2/"), end) < 0)
	// Output:
	// scan [/rad/data/t1/, /rad/data/t10)
	// true
	// false
}
