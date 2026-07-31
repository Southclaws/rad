//! Spec-first command-line assembly outside the database engine.

mod client;
mod commands;
mod doctor;
pub mod generated;
mod init;
mod output;
mod project;
mod skills;
mod spec;
mod state;

use std::process::ExitCode;

use clap::Parser;

pub async fn run() -> ExitCode {
    let cli = generated::Cli::parse();
    let json = output::is_json(&cli.globals);
    match cli.dispatch(&mut commands::App).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            output::render_error(error.as_ref(), json);
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use clap::Parser;

    use super::generated::*;
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

    #[test]
    fn generated_init_supports_the_fast_non_interactive_path() {
        let cli = Cli::try_parse_from([
            "rad",
            "init",
            "project",
            "--yes",
            "--empty",
            "--no-generate",
            "--database-url",
            "rads://db.example.com",
        ])
        .unwrap();
        let RootCommand::Init(init) = cli.command else {
            panic!("expected init command");
        };
        assert_eq!(init.directory, std::path::Path::new("project"));
        assert_eq!(init.database_url, "rads://db.example.com");
        assert!(init.yes);
        assert!(init.empty);
        assert!(init.no_generate);
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

        async fn init(&mut self, _: &GlobalArgs, _: InitArgs) -> Result<(), Self::Error> {
            unreachable!()
        }

        async fn serve(&mut self, _: &GlobalArgs, _: ServeArgs) -> Result<(), Self::Error> {
            unreachable!()
        }

        async fn validate(&mut self, _: &GlobalArgs, _: ValidateArgs) -> Result<(), Self::Error> {
            unreachable!()
        }

        async fn doctor(&mut self, _: &GlobalArgs, _: DoctorArgs) -> Result<(), Self::Error> {
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

        async fn schema_json_schema(
            &mut self,
            _: &GlobalArgs,
            _: &SchemaOptions,
            _: SchemaJsonSchemaArgs,
        ) -> Result<(), Self::Error> {
            unreachable!()
        }

        async fn schema_transitions_list(
            &mut self,
            _: &GlobalArgs,
            _: &SchemaOptions,
            _: &SchemaTransitionsOptions,
            _: SchemaTransitionsListArgs,
        ) -> Result<(), Self::Error> {
            unreachable!()
        }

        async fn schema_transitions_get(
            &mut self,
            _: &GlobalArgs,
            _: &SchemaOptions,
            _: &SchemaTransitionsOptions,
            _: SchemaTransitionsGetArgs,
        ) -> Result<(), Self::Error> {
            unreachable!()
        }

        async fn schema_transitions_wait(
            &mut self,
            _: &GlobalArgs,
            _: &SchemaOptions,
            _: &SchemaTransitionsOptions,
            _: SchemaTransitionsWaitArgs,
        ) -> Result<(), Self::Error> {
            unreachable!()
        }

        async fn schema_transitions_cancel(
            &mut self,
            _: &GlobalArgs,
            _: &SchemaOptions,
            _: &SchemaTransitionsOptions,
            _: SchemaTransitionsCancelArgs,
        ) -> Result<(), Self::Error> {
            unreachable!()
        }

        async fn generate(&mut self, _: &GlobalArgs, _: GenerateArgs) -> Result<(), Self::Error> {
            unreachable!()
        }

        async fn skills_list(
            &mut self,
            _: &GlobalArgs,
            _: &SkillsOptions,
            _: SkillsListArgs,
        ) -> Result<(), Self::Error> {
            unreachable!()
        }

        async fn skills_get(
            &mut self,
            _: &GlobalArgs,
            _: &SkillsOptions,
            _: SkillsGetArgs,
        ) -> Result<(), Self::Error> {
            unreachable!()
        }

        async fn skills_path(
            &mut self,
            _: &GlobalArgs,
            _: &SkillsOptions,
            _: SkillsPathArgs,
        ) -> Result<(), Self::Error> {
            unreachable!()
        }

        async fn spec(&mut self, _: &GlobalArgs, _: SpecArgs) -> Result<(), Self::Error> {
            unreachable!()
        }
    }
}
