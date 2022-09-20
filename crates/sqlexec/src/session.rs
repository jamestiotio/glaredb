use crate::catalog::{DatabaseCatalog, DEFAULT_SCHEMA};
use crate::datasource::MemTable;
use crate::errors::{internal, Result};
use crate::logical_plan::*;
use datafusion::arrow::datatypes::{Field, Schema};
use datafusion::catalog::catalog::CatalogList;
use datafusion::catalog::schema::SchemaProvider;
use datafusion::datasource::listing::{ListingTable, ListingTableConfig, ListingTableUrl};
use datafusion::execution::context::{SessionConfig, SessionState, TaskContext};
use datafusion::execution::options::ParquetReadOptions;
use datafusion::execution::runtime_env::RuntimeEnv;
use datafusion::logical_plan::LogicalPlan as DfLogicalPlan;
use datafusion::physical_plan::{
    coalesce_partitions::CoalescePartitionsExec, EmptyRecordBatchStream, ExecutionPlan,
    SendableRecordBatchStream,
};
use datafusion::sql::planner::{convert_data_type, SqlToRel};
use datafusion::sql::sqlparser::ast;
use datafusion::sql::{ResolvedTableReference, TableReference};
use futures::StreamExt;
use std::sync::Arc;
use tracing::debug;

/// A per-client user session.
///
/// This type acts as the bridge between datafusion planning/execution and the
/// rest of the system.
pub struct Session {
    /// Datafusion state for query planning and execution.
    state: SessionState,

    /// The concretely typed "GlareDB" catalog.
    catalog: Arc<DatabaseCatalog>,
    // TODO: Transaction context goes here.
}

impl Session {
    /// Create a new session.
    ///
    /// All system schemas (including `information_schema`) should already be in
    /// the provided catalog.
    pub fn new(catalog: Arc<DatabaseCatalog>, runtime: Arc<RuntimeEnv>) -> Session {
        // Note that the values for creating the default catalog/schema and
        // information schema do not matter, as we're not using the datafusion's
        // session context. Initializing the session context is what creates
        // thoses.
        let config = SessionConfig::default()
            .with_default_catalog_and_schema(catalog.name(), DEFAULT_SCHEMA)
            .create_default_catalog_and_schema(false)
            .with_information_schema(false);

        let mut state = SessionState::with_config_rt(config, runtime);
        state.catalog_list = catalog.clone();

        Session { state, catalog }
    }

    pub(crate) fn plan_sql(&self, statement: ast::Statement) -> Result<LogicalPlan> {
        debug!(%statement, "planning sql statement");
        let planner = SqlToRel::new(&self.state);

        match statement {
            ast::Statement::StartTransaction { .. } => Ok(TransactionPlan::Begin.into()),
            ast::Statement::Commit { .. } => Ok(TransactionPlan::Commit.into()),
            ast::Statement::Rollback { .. } => Ok(TransactionPlan::Abort.into()),

            ast::Statement::Query(query) => {
                let plan = planner.query_to_plan(*query, &mut hashbrown::HashMap::new())?;
                Ok(LogicalPlan::Query(plan))
            }

            ast::Statement::Explain {
                analyze,
                verbose,
                statement,
                ..
            } => {
                let plan = planner.explain_statement_to_plan(verbose, analyze, *statement)?;
                Ok(LogicalPlan::Query(plan))
            }

            ast::Statement::CreateSchema {
                schema_name,
                if_not_exists,
            } => Ok(DdlPlan::CreateSchema(CreateSchema {
                schema_name: schema_name.to_string(),
                if_not_exists,
            })
            .into()),

            // Normal tables.
            ast::Statement::CreateTable {
                external: false,
                if_not_exists,
                name,
                columns,
                query: None,
                ..
            } => {
                let mut arrow_cols = Vec::with_capacity(columns.len());
                for column in columns.into_iter() {
                    let dt = convert_data_type(&column.data_type)?;
                    let field = Field::new(&column.name.value, dt, false);
                    arrow_cols.push(field);
                }

                Ok(DdlPlan::CreateTable(CreateTable {
                    table_name: name.to_string(),
                    columns: arrow_cols,
                    if_not_exists,
                })
                .into())
            }

            // External tables.
            ast::Statement::CreateTable {
                external: true,
                if_not_exists: false,
                name,
                file_format: Some(file_format),
                location: Some(location),
                query: None,
                ..
            } => {
                let file_type: FileType = file_format.try_into()?;
                Ok(DdlPlan::CreateExternalTable(CreateExternalTable {
                    table_name: name.to_string(),
                    location,
                    file_type,
                })
                .into())
            }

            // Tables generated from a source query.
            //
            // CREATE TABLE table2 AS (SELECT * FROM table1);
            ast::Statement::CreateTable {
                external: false,
                name,
                query: Some(query),
                ..
            } => {
                let source = planner.query_to_plan(*query, &mut hashbrown::HashMap::new())?;
                Ok(DdlPlan::CreateTableAs(CreateTableAs {
                    table_name: name.to_string(),
                    source,
                })
                .into())
            }

            ast::Statement::Insert {
                table_name,
                columns,
                source,
                ..
            } => {
                let table_name = table_name.to_string();
                let columns = columns.into_iter().map(|col| col.value).collect();

                let source = planner.query_to_plan(*source, &mut hashbrown::HashMap::new())?;

                Ok(WritePlan::Insert(Insert {
                    table_name,
                    columns,
                    source,
                })
                .into())
            }

            stmt => Err(internal!("unsupported sql statement: {}", stmt)),
        }
    }

    pub(crate) async fn create_physical_plan(
        &self,
        plan: DfLogicalPlan,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        let plan = self.state.create_physical_plan(&plan).await?;
        Ok(plan)
    }

    pub(crate) fn execute_physical(
        &self,
        plan: Arc<dyn ExecutionPlan>,
    ) -> Result<SendableRecordBatchStream> {
        let context = Arc::new(TaskContext::from(&self.state));
        match plan.output_partitioning().partition_count() {
            0 => Ok(Box::pin(EmptyRecordBatchStream::new(plan.schema()))),
            1 => Ok(plan.execute(0, context)?),
            _ => {
                let plan = CoalescePartitionsExec::new(plan);
                Ok(plan.execute(0, context)?)
            }
        }
    }

    pub(crate) fn create_table(&self, plan: CreateTable) -> Result<()> {
        let table_ref: TableReference = plan.table_name.as_str().into();
        let resolved = table_ref.resolve(self.catalog.name(), DEFAULT_SCHEMA);

        let catalog = self
            .catalog
            .catalog(resolved.catalog)
            .ok_or_else(|| internal!("missing catalog: {}", resolved.catalog))?;
        let schema = catalog
            .schema(resolved.schema)
            .ok_or_else(|| internal!("missing schema: {}", resolved.schema))?;

        // TODO: If not exists

        let table_schema = Schema::new(plan.columns);
        let mem_table = MemTable::new(Arc::new(table_schema));

        schema.register_table(resolved.table.to_string(), Arc::new(mem_table))?;

        Ok(())
    }

    pub(crate) async fn create_external_table(&self, plan: CreateExternalTable) -> Result<()> {
        let resolved = self.resolve_table_name(&plan.table_name);
        let schema = self.get_schema_for_reference(&resolved)?;

        let target_partitions = self.state.config.target_partitions;
        let opts = match plan.file_type {
            FileType::Parquet => {
                ParquetReadOptions::default().to_listing_options(target_partitions)
            }
        };
        let path = ListingTableUrl::parse(&plan.location)?;
        let file_schema = opts.infer_schema(&self.state, &path).await?;
        let config = ListingTableConfig::new(path)
            .with_listing_options(opts)
            .with_schema(file_schema);

        let table = ListingTable::try_new(config)?;
        schema.register_table(resolved.table.to_string(), Arc::new(table))?;

        Ok(())
    }

    pub(crate) async fn create_table_as(&self, plan: CreateTableAs) -> Result<()> {
        let resolved = self.resolve_table_name(&plan.table_name);
        let schema = self.get_schema_for_reference(&resolved)?;

        // Plan and execute the source. We'll use the first batch from the
        // stream to create the table with the correct schema.
        let physical = self.create_physical_plan(plan.source).await?;
        let mut stream = self.execute_physical(physical)?;

        let table = match stream.next().await {
            Some(result) => {
                let batch = result?;
                let schema = batch.schema();
                let table = MemTable::new(schema);
                table.insert_batch(batch)?;
                table
            }
            None => {
                return Err(internal!(
                    "source stream empty, cannot infer schema from empty stream"
                ))
            }
        };

        // Insert the rest of the stream.
        while let Some(result) = stream.next().await {
            let batch = result?;
            table.insert_batch(batch)?;
        }

        // Finally register the table.
        schema.register_table(resolved.table.to_string(), Arc::new(table))?;

        Ok(())
    }

    pub(crate) async fn insert(&self, plan: Insert) -> Result<()> {
        let resolved = self.resolve_table_name(&plan.table_name);
        let schema = self.get_schema_for_reference(&resolved)?;

        let table = schema
            .table(resolved.table)
            .ok_or_else(|| internal!("missing table: {}", resolved.table))?;

        let table = table
            .as_any()
            .downcast_ref::<MemTable>()
            .ok_or_else(|| internal!("cannot downcast to mem table"))?;

        let physical = self.create_physical_plan(plan.source).await?;
        let mut stream = self.execute_physical(physical)?;

        let schema = stream.schema();
        debug!(?schema, "inserting with schema");

        while let Some(result) = stream.next().await {
            let result = result?;
            table.insert_batch(result)?;
        }

        Ok(())
    }

    fn resolve_table_name<'a>(&'a self, table_name: &'a str) -> ResolvedTableReference<'a> {
        let table_ref: TableReference = table_name.into();
        table_ref.resolve(self.catalog.name(), DEFAULT_SCHEMA)
    }

    /// Get a schema provider given some resolved table reference.
    fn get_schema_for_reference(
        &self,
        resolved: &ResolvedTableReference,
    ) -> Result<Arc<dyn SchemaProvider>> {
        let catalog = self
            .catalog
            .catalog(resolved.catalog)
            .ok_or_else(|| internal!("missing catalog: {}", resolved.catalog))?;
        let schema = catalog
            .schema(resolved.schema)
            .ok_or_else(|| internal!("missing schema: {}", resolved.schema))?;
        Ok(schema)
    }
}
