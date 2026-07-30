use super::*;
use crate::engine::catalog::model::{Column, ColumnDraft};

impl Mutation<'_> {
    pub async fn table_by_schema_id(&mut self, id: SchemaId) -> Result<Table> {
        store::get_table_by_schema_id(self.view, id)
            .await?
            .ok_or_else(|| input(format!("catalog: table schema ID {id} does not exist")))
    }

    pub async fn column_by_schema_id(
        &mut self,
        table_id: SchemaId,
        column_id: SchemaId,
    ) -> Result<(Table, Column)> {
        let table = self.table_by_schema_id(table_id).await?;
        let column = table
            .columns
            .iter()
            .find(|column| column.schema_id == column_id)
            .cloned()
            .ok_or_else(|| {
                input(format!(
                    "catalog: column schema ID {column_id} does not exist on table schema ID {table_id}"
                ))
            })?;
        Ok((table, column))
    }

    pub async fn rename_table_by_schema_id(&mut self, table_id: SchemaId, to: &str) -> Result<()> {
        let table = self.table_by_schema_id(table_id).await?;
        self.rename_table(&table.name, to).await
    }

    pub async fn delete_table_by_schema_id(&mut self, table_id: SchemaId) -> Result<()> {
        let table = self.table_by_schema_id(table_id).await?;
        self.delete_table(&table.name).await
    }

    pub async fn create_column_by_schema_id(
        &mut self,
        table_id: SchemaId,
        definition: ColumnDraft,
    ) -> Result<Table> {
        let table = self.table_by_schema_id(table_id).await?;
        self.create_column(&table.name, definition).await
    }

    pub async fn rename_column_by_schema_id(
        &mut self,
        table_id: SchemaId,
        column_id: SchemaId,
        to: &str,
    ) -> Result<Table> {
        let (table, column) = self.column_by_schema_id(table_id, column_id).await?;
        self.rename_column(&table.name, &column.name, to).await
    }

    pub async fn change_column_insert_default_by_schema_id(
        &mut self,
        table_id: SchemaId,
        column_id: SchemaId,
        value: Option<super::super::model::DefaultValue>,
    ) -> Result<Table> {
        let (table, column) = self.column_by_schema_id(table_id, column_id).await?;
        self.change_column_insert_default(&table.name, &column.name, value)
            .await
    }

    pub async fn delete_column_by_schema_id(
        &mut self,
        table_id: SchemaId,
        column_id: SchemaId,
    ) -> Result<Table> {
        let (table, column) = self.column_by_schema_id(table_id, column_id).await?;
        self.delete_column(&table.name, &column.name).await
    }

    pub async fn delete_index_by_schema_id(
        &mut self,
        table_id: SchemaId,
        index: &str,
    ) -> Result<()> {
        let table = self.table_by_schema_id(table_id).await?;
        self.delete_index(&table.name, index).await
    }
}
