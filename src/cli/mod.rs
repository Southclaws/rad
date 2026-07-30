//! Spec-first command-line assembly outside the database engine.

mod client;
mod commands;
pub mod generated;
mod project;
mod state;

use clap::Parser;

use crate::process::Result;

pub async fn run() -> Result {
    generated::Cli::parse().dispatch(&mut commands::App).await
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use super::generated::*;
    use clap::Parser;

    #[test]
    fn generated_parser_preserves_parent_schema_options() {
        let cli = Cli::try_parse_from([
            "rad",
            "schema",
            "--config",
            "project/rad.config.yaml",
            "--file",
            "project/schema.yaml",
            "diff",
            "--format",
            "json",
        ])
        .unwrap();
        let RootCommand::Schema(schema) = cli.command else {
            panic!("expected schema command");
        };
        assert_eq!(
            schema.options.config,
            std::path::Path::new("project/rad.config.yaml")
        );
        assert_eq!(
            schema.options.file,
            std::path::Path::new("project/schema.yaml")
        );
        assert!(matches!(schema.command, SchemaCommand::Diff(_)));
    }

    #[test]
    fn generated_serve_flags_are_strongly_typed() {
        let cli = Cli::try_parse_from(["rad", "serve", "--storage", "memory"]).unwrap();
        let RootCommand::Serve(serve) = cli.command else {
            panic!("expected serve command");
        };
        assert_eq!(serve.storage, ServeStorage::Memory);
    }

    #[tokio::test]
    async fn generated_dispatch_passes_parent_options_to_the_leaf_handler() {
        let cli = Cli::try_parse_from([
            "rad",
            "schema",
            "--config",
            "nested/rad.config.yaml",
            "diff",
        ])
        .unwrap();
        let mut handler = RecordingHandler::default();
        cli.dispatch(&mut handler).await.unwrap();
        assert_eq!(
            handler.schema_config.as_deref(),
            Some(std::path::Path::new("nested/rad.config.yaml"))
        );
    }

    #[derive(Default)]
    struct RecordingHandler {
        schema_config: Option<std::path::PathBuf>,
    }

    impl Handler for RecordingHandler {
        type Error = Infallible;

        async fn serve(&mut self, _: &GlobalArgs, _: ServeArgs) -> Result<(), Self::Error> {
            unreachable!()
        }

        async fn validate(&mut self, _: &GlobalArgs, _: ValidateArgs) -> Result<(), Self::Error> {
            unreachable!()
        }

        async fn schema_status(
            &mut self,
            _: &GlobalArgs,
            _: &SchemaOptions,
            _: SchemaStatusArgs,
        ) -> Result<(), Self::Error> {
            unreachable!()
        }

        async fn schema_diff(
            &mut self,
            _: &GlobalArgs,
            schema: &SchemaOptions,
            _: SchemaDiffArgs,
        ) -> Result<(), Self::Error> {
            self.schema_config = Some(schema.config.clone());
            Ok(())
        }

        async fn schema_migrate(
            &mut self,
            _: &GlobalArgs,
            _: &SchemaOptions,
            _: SchemaMigrateArgs,
        ) -> Result<(), Self::Error> {
            unreachable!()
        }

        async fn schema_pull(
            &mut self,
            _: &GlobalArgs,
            _: &SchemaOptions,
            _: SchemaPullArgs,
        ) -> Result<(), Self::Error> {
            unreachable!()
        }

        async fn generate(&mut self, _: &GlobalArgs, _: GenerateArgs) -> Result<(), Self::Error> {
            unreachable!()
        }
    }
}
