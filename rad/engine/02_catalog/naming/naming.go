package naming

import "strings"

func Index(table string, columns []string, unique bool) string {
	suffix := "idx"
	if unique {
		suffix = "uq"
	}
	return table + "_" + strings.Join(columns, "_") + "_" + suffix
}

func ForeignKey(table, column string) string {
	return table + "_" + column + "_fk"
}

func NotNullConstraint(table, column string) string {
	return table + "_" + column + "_not_null"
}
