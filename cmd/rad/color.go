package main

import "os"

// useColor is decided once at startup: honor NO_COLOR, allow a forced override
// for pipes/CI, otherwise only colorize when stdout is a real terminal.
var useColor = wantColor()

func wantColor() bool {
	if os.Getenv("NO_COLOR") != "" {
		return false
	}
	if os.Getenv("CLICOLOR_FORCE") != "" || os.Getenv("FORCE_COLOR") != "" {
		return true
	}
	if os.Getenv("TERM") == "dumb" {
		return false
	}
	fi, err := os.Stdout.Stat()
	if err != nil {
		return false
	}
	return fi.Mode()&os.ModeCharDevice != 0
}

func paint(code, s string) string {
	if !useColor {
		return s
	}
	return code + s + "\x1b[0m"
}

// Phosphor palette (truecolor), matching the brand's green-on-ink.
func cGreen(s string) string { return paint("\x1b[38;2;92;242;155m", s) }
func cDim(s string) string   { return paint("\x1b[38;2;138;153;145m", s) }
func cBold(s string) string  { return paint("\x1b[1m", s) }
func cRed(s string) string   { return paint("\x1b[38;2;240;120;110m", s) }
