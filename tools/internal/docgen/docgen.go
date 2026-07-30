// Package docgen contains the small formatting helpers shared by Rad's
// reader-facing documentation generators.
package docgen

import (
	"bytes"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"text/template"
)

var markdownLink = regexp.MustCompile(`\[([^\]]+)\]\(([^)]+)\)`)

// Render executes a documentation template with the standard helpers.
func Render(name, source string, data any, extra template.FuncMap) ([]byte, error) {
	funcs := template.FuncMap{
		"code":        Code,
		"inline":      Inline,
		"oneLine":     OneLine,
		"paragraphs":  Paragraphs,
		"table":       Table,
		"withoutDash": WithoutDash,
	}
	for key, value := range extra {
		funcs[key] = value
	}

	tmpl, err := template.New(name).Funcs(funcs).Option("missingkey=error").Parse(source)
	if err != nil {
		return nil, fmt.Errorf("parse template: %w", err)
	}

	var output bytes.Buffer
	if err := tmpl.Execute(&output, data); err != nil {
		return nil, fmt.Errorf("execute template: %w", err)
	}
	return output.Bytes(), nil
}

// Write creates the parent directory and writes a generated document.
func Write(path string, data []byte) error {
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return fmt.Errorf("create output directory: %w", err)
	}
	if err := os.WriteFile(path, data, 0o644); err != nil {
		return fmt.Errorf("write %s: %w", path, err)
	}
	return nil
}

// OneLine collapses prose into one line for summaries and tables.
func OneLine(value string) string {
	return strings.Join(strings.Fields(WithoutDash(value)), " ")
}

// Paragraphs normalises line endings and removes unsupported punctuation while
// preserving authored paragraph breaks and Markdown.
func Paragraphs(value string) string {
	value = strings.ReplaceAll(value, "\r\n", "\n")
	value = strings.TrimSpace(WithoutDash(value))
	for strings.Contains(value, "\n\n\n") {
		value = strings.ReplaceAll(value, "\n\n\n", "\n\n")
	}
	return value
}

// Inline protects prose used inside Markdown tables.
func Inline(value string) string {
	value = OneLine(value)
	value = strings.ReplaceAll(value, "|", `\|`)
	return value
}

// Table protects Markdown links while escaping every other table separator.
func Table(value string) string {
	links := markdownLink.FindAllString(value, -1)
	for index, link := range links {
		value = strings.Replace(value, link, fmt.Sprintf("\x00%d\x00", index), 1)
	}
	value = Inline(value)
	for index, link := range links {
		value = strings.ReplaceAll(value, fmt.Sprintf("\x00%d\x00", index), link)
	}
	return value
}

// Code formats a value as inline code.
func Code(value any) string {
	if value == nil || fmt.Sprint(value) == "" {
		return ""
	}
	text := strings.ReplaceAll(fmt.Sprint(value), "`", "\\`")
	text = strings.ReplaceAll(text, "|", `\|`)
	return "`" + text + "`"
}

// WithoutDash keeps generated documents consistent with the site's punctuation
// style. The source specifications remain unchanged.
func WithoutDash(value string) string {
	value = strings.ReplaceAll(value, " — ", "; ")
	value = strings.ReplaceAll(value, "—", "-")
	return value
}
