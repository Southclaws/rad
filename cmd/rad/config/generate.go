package config

//go:generate go run github.com/Southclaws/rad/tools/schemagen -yaml rad.config.schema.yaml -json rad.config.schema.json
//go:generate go run github.com/Southclaws/schemancer@latest rad.config.schema.json golang . --package config --optional-style pointer
