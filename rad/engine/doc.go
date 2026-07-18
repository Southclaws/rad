// Package engine is Rad's core relational database engine.
//
// Layer architecture: yes the numbers are a little odd.
//
//	01_kv       storage layer, key-value store and key encodings.
//
//	02_catalog  catalog façade, with model, store, change, schema,
//	            migrate, and naming
//
//	03_lir      relation-graph values and nodes, with bound, eval,
//	            format, and inspect
//
//	04_planner  binding, analysis, physical nodes, plan construction,
//	            program binding, and explain output
//
//	05_exec     transaction façade, with codec, rowstore, query, mutate,
//	            program, differential, generative, and refexec
//
//	06_frontend application façade, with catalog api, migration, and
//	            result modelling
//
// Symbols are shared upward, never downward.
package engine
