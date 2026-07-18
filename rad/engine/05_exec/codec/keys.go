package codec

// Data and index key construction. Top-level namespaces are literal path
// strings; tuple segments use order-preserving encodings from 01_kv/keyenc.
//
//	/rad/data/{table_id}/primary/{pk_tuple}
//	/rad/index/{table_id}/{index_id}/{indexed_tuple}{pk_tuple}
//
// Note the indexed tuple and pk tuple are concatenated without a separator:
// tuple encodings are self-delimiting, so the boundary is unambiguous and a
// separator would break ordering.
//
// Exported so tooling (cmd/rad) can build and decode these keys.

func DataPrefix(tableID string) []byte {
	return []byte("/rad/data/" + tableID + "/primary/")
}

func DataKey(tableID string, pkTuple []byte) []byte {
	return append(DataPrefix(tableID), pkTuple...)
}

func IndexPrefix(tableID, indexID string) []byte {
	return []byte("/rad/index/" + tableID + "/" + indexID + "/")
}

func IndexKey(tableID, indexID string, indexedTuple, pkTuple []byte) []byte {
	k := append(IndexPrefix(tableID, indexID), indexedTuple...)
	return append(k, pkTuple...)
}
