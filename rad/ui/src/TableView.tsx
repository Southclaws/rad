import { useEffect, useMemo, useState } from 'react'
import { publicAPI, type ColumnInfo, type ForeignKeyInfo, type IndexInfo, type TableInfo } from './api'

type Tab = 'data' | 'columns' | 'indexes' | 'table'
type DraftColumn = ColumnInfo & { nullable: boolean }

const columnTypes: ColumnInfo['type'][] = ['text', 'int64', 'float64', 'bool']

export function TableView({
  table,
  mutable,
  onChanged,
  onDeleted,
}: {
  table: TableInfo
  mutable: boolean
  onChanged: (table: TableInfo) => void
  onDeleted: () => void
}) {
  const [tab, setTab] = useState<Tab>('data')
  const [error, setError] = useState<string | null>(null)

  const changed = (next: TableInfo) => {
    setError(null)
    onChanged(next)
  }

  return (
    <div className="table-view">
      <div className="table-view__header">
        <div>
          <p className="section-label">table</p>
          <h1>{table.name}</h1>
          <p className="table-view__description">Read through the public query API; change catalog metadata through the public catalog API.</p>
        </div>
        {!mutable && <div className="table-view__mode-notice">Schema-managed database — mutate this table through schema migrations.</div>}
      </div>
      <div className="table-view__tabs" role="tablist" aria-label={`${table.name} views`}>
        {(['data', 'columns', 'indexes', 'table'] as Tab[]).map((name) => (
          <button key={name} className={`table-view__tab ${tab === name ? 'table-view__tab--active' : ''}`} onClick={() => setTab(name)} role="tab" aria-selected={tab === name}>
            {name}
          </button>
        ))}
      </div>
      {error && <div className="ui-notice ui-notice--error" role="alert">{error}</div>}
      {tab === 'data' && <DataTab table={table} setError={setError} />}
      {tab === 'columns' && <ColumnsTab table={table} mutable={mutable} onChanged={changed} setError={setError} />}
      {tab === 'indexes' && <IndexesTab table={table} mutable={mutable} onChanged={changed} setError={setError} />}
      {tab === 'table' && <TableSettings table={table} mutable={mutable} onChanged={changed} onDeleted={onDeleted} setError={setError} />}
    </div>
  )
}

function DataTab({ table, setError }: { table: TableInfo; setError: (message: string | null) => void }) {
  const [rows, setRows] = useState<Record<string, unknown>[]>([])
  const [loading, setLoading] = useState(true)

  useEffect(() => {
    let cancelled = false
    setLoading(true)
    setError(null)
    publicAPI.query(tableQuery(table)).then(({ result }) => {
      if (!cancelled) setRows(Array.isArray(result) ? result as Record<string, unknown>[] : [])
    }).catch((e) => !cancelled && setError(String(e))).finally(() => !cancelled && setLoading(false))
    return () => { cancelled = true }
  }, [table, setError])

  return (
    <section className="data-table">
      <div className="data-table__toolbar">
        <div><strong>{loading ? 'Loading rows…' : `${rows.length} rows`}</strong><span>First 250 rows ordered by primary key.</span></div>
        <code>POST /query</code>
      </div>
      <div className="data-table__scroll">
        <table className="data-table__grid">
          <thead><tr>{table.columns.map((column) => <th key={column.name}>{column.name}<span className="data-table__type">{column.type}{column.nullable ? '?' : ''}</span></th>)}</tr></thead>
          <tbody>
            {rows.map((row, index) => <tr key={index}>{table.columns.map((column) => <td key={column.name} className={row[column.name] == null ? 'data-table__cell--null' : ''}>{displayValue(row[column.name])}</td>)}</tr>)}
          </tbody>
        </table>
        {!loading && rows.length === 0 && <div className="empty-state">No rows returned.</div>}
      </div>
    </section>
  )
}

function tableQuery(table: TableInfo) {
  const terms = table.primary_key.map((column) => ({ expr: { kind: 'col', scope: 'row', column } }))
  const nodes: Record<string, unknown> = {
    scan: { kind: 'scan', table: table.name, scope: 'row' },
    project: { kind: 'project', input: table.primary_key.length ? 'slice' : 'scan', spread: ['row'] },
  }
  if (table.primary_key.length) {
    nodes.order = { kind: 'order', input: 'scan', terms }
    nodes.slice = { kind: 'slice', input: 'order', limit: 250 }
  }
  return { nodes, root: { node: 'project', cardinality: 'many' } }
}

function ColumnsTab({ table, mutable, onChanged, setError }: TabProps) {
  const [draft, setDraft] = useState<DraftColumn>({ name: '', type: 'text', nullable: true })
  const pk = new Set(table.primary_key)
  const submit = async () => {
    try { onChanged(await publicAPI.createColumn(table.name, draft)) } catch (e) { setError(String(e)) }
  }
  return <section className="catalog-editor">
    <div className="catalog-editor__toolbar"><div><strong>Columns</strong><span>Rename and delete are metadata operations; creation follows the catalog’s backfill rules.</span></div><code>POST /tables/&#123;table&#125;/columns</code></div>
    <div className="catalog-editor__list">
      {table.columns.map((column) => <ColumnEditor key={column.name} column={column} primaryKey={pk.has(column.name)} table={table.name} mutable={mutable} onChanged={onChanged} setError={setError} />)}
    </div>
    {mutable && <div className="catalog-editor__add-row">
      <input aria-label="New column name" placeholder="column name" value={draft.name} onChange={(e) => setDraft({ ...draft, name: e.target.value })} />
      <TypeSelect value={draft.type} onChange={(type) => setDraft({ ...draft, type })} />
      <label className="form-check"><input type="checkbox" checked={draft.nullable} onChange={(e) => setDraft({ ...draft, nullable: e.target.checked })} /> nullable</label>
      <button className="button button--primary" onClick={submit}>Add column</button>
    </div>}
  </section>
}

function ColumnEditor({ column, primaryKey, table, mutable, onChanged, setError }: { column: ColumnInfo; primaryKey: boolean; table: string } & Omit<TabProps, 'table'>) {
  const [name, setName] = useState(column.name)
  const rename = async () => { try { onChanged(await publicAPI.renameColumn(table, column.name, name)) } catch (e) { setError(String(e)) } }
  const remove = async () => { try { onChanged(await publicAPI.deleteColumn(table, column.name)) } catch (e) { setError(String(e)) } }
  return <div className="catalog-editor__row">
    <input aria-label={`Name for ${column.name}`} value={name} onChange={(e) => setName(e.target.value)} disabled={!mutable} />
    <span className="catalog-editor__type">{column.type}</span>
    <span className="text-muted">{column.nullable ? 'nullable' : 'required'}{primaryKey ? ' · primary key' : ''}</span>
    {mutable && <div className="catalog-editor__actions"><button className="button" disabled={name === column.name} onClick={rename}>Rename</button><button className="button button--danger" onClick={remove}>Delete</button></div>}
  </div>
}

function IndexesTab({ table, mutable, onChanged, setError }: TabProps) {
  const [draft, setDraft] = useState<IndexInfo>({ name: '', columns: [], unique: false })
  const create = async () => { try { onChanged(await publicAPI.createIndex(table.name, draft)) } catch (e) { setError(String(e)) } }
  return <section className="catalog-editor">
    <div className="catalog-editor__toolbar"><div><strong>Indexes</strong><span>Replacing an index deletes then recreates it. It is intentionally lossy until an update endpoint exists.</span></div><code>POST /tables/&#123;table&#125;/indexes</code></div>
    <div className="catalog-editor__list">
      {(table.indexes ?? []).map((index) => <IndexEditor key={index.name} index={index} table={table.name} mutable={mutable} onChanged={onChanged} setError={setError} />)}
      {(table.indexes ?? []).length === 0 && <div className="empty-state">No secondary indexes.</div>}
    </div>
    {mutable && <div className="catalog-editor__add-row">
      <input aria-label="New index name" placeholder="index name" value={draft.name} onChange={(e) => setDraft({ ...draft, name: e.target.value })} />
      <input aria-label="New index columns" placeholder="columns, in order" value={draft.columns.join(', ')} onChange={(e) => setDraft({ ...draft, columns: splitList(e.target.value) })} />
      <label className="form-check"><input type="checkbox" checked={draft.unique} onChange={(e) => setDraft({ ...draft, unique: e.target.checked })} /> unique</label>
      <button className="button button--primary" onClick={create}>Add index</button>
    </div>}
  </section>
}

function IndexEditor({ index, table, mutable, onChanged, setError }: { index: IndexInfo; table: string } & Omit<TabProps, 'table'>) {
  const [draft, setDraft] = useState<IndexInfo>({ ...index, columns: [...index.columns] })
  const changed = useMemo(() => draft.name !== index.name || draft.unique !== index.unique || draft.columns.join(',') !== index.columns.join(','), [draft, index])
  const replace = async () => {
    try {
      await publicAPI.deleteIndex(table, index.name)
      onChanged(await publicAPI.createIndex(table, draft))
    } catch (e) { setError(String(e)) }
  }
  const remove = async () => { try { onChanged(await publicAPI.deleteIndex(table, index.name)) } catch (e) { setError(String(e)) } }
  return <div className="catalog-editor__row catalog-editor__row--index">
    <input aria-label={`Name for ${index.name}`} value={draft.name} onChange={(e) => setDraft({ ...draft, name: e.target.value })} disabled={!mutable} />
    <input aria-label={`Columns for ${index.name}`} value={draft.columns.join(', ')} onChange={(e) => setDraft({ ...draft, columns: splitList(e.target.value) })} disabled={!mutable} />
    <label className="form-check"><input type="checkbox" checked={Boolean(draft.unique)} disabled={!mutable} onChange={(e) => setDraft({ ...draft, unique: e.target.checked })} /> unique</label>
    {mutable && <div className="catalog-editor__actions"><button className="button" disabled={!changed} onClick={replace}>Replace</button><button className="button button--danger" onClick={remove}>Delete</button></div>}
  </div>
}

function TableSettings({ table, mutable, onChanged, onDeleted, setError }: TabProps & { onDeleted: () => void }) {
  const [name, setName] = useState(table.name)
  const rename = async () => { try { onChanged(await publicAPI.renameTable(table.name, name)) } catch (e) { setError(String(e)) } }
  const remove = async () => { if (window.confirm(`Delete table ${table.name}? Its rows become unreachable.`)) try { await publicAPI.deleteTable(table.name); onDeleted() } catch (e) { setError(String(e)) } }
  return <section className="table-settings">
    <div className="table-settings__item"><div><h2>Table name</h2><p>Renaming changes catalog metadata only; rows keep their stable table identity.</p></div><div className="table-settings__form"><input value={name} disabled={!mutable} onChange={(e) => setName(e.target.value)} /><button className="button button--primary" disabled={!mutable || name === table.name} onClick={rename}>Rename table</button></div></div>
    {mutable && <div className="table-settings__item table-settings__item--danger"><div><h2>Delete table</h2><p>Deletes the catalog entry. Existing rows and index entries become unreachable.</p></div><button className="button button--danger" onClick={remove}>Delete table</button></div>}
  </section>
}

export function NewTableView({ onCreated }: { onCreated: (table: TableInfo) => void }) {
  const [name, setName] = useState('')
  const [columns, setColumns] = useState<DraftColumn[]>([{ name: 'id', type: 'text', nullable: false }])
  const [primaryKey, setPrimaryKey] = useState('id')
  const [indexes, setIndexes] = useState<IndexInfo[]>([])
  const [foreignKeys, setForeignKeys] = useState<ForeignKeyInfo[]>([])
  const [error, setError] = useState<string | null>(null)
  const [submitting, setSubmitting] = useState(false)
  const create = async () => {
    setSubmitting(true); setError(null)
    try { onCreated(await publicAPI.createTable({ name, columns, primary_key: splitList(primaryKey), indexes, foreign_keys: foreignKeys })) } catch (e) { setError(String(e)) } finally { setSubmitting(false) }
  }
  return <section className="catalog-form">
    <div className="catalog-form__header"><div><p className="section-label">catalog</p><h1>Create table</h1><p className="catalog-form__description">The complete initial definition is sent atomically to <code>POST /tables</code>.</p></div></div>
    {error && <div className="ui-notice ui-notice--error" role="alert">{error}</div>}
    <div className="catalog-form__card">
      <label className="catalog-form__field">Table name<input value={name} placeholder="users" onChange={(e) => setName(e.target.value)} /></label>
      <div className="catalog-form__section"><h2>Columns</h2>{columns.map((column, i) => <div className="catalog-form__row" key={i}><input aria-label="Column name" value={column.name} placeholder="column name" onChange={(e) => setColumns(columns.map((c, n) => n === i ? { ...c, name: e.target.value } : c))} /><TypeSelect value={column.type} onChange={(type) => setColumns(columns.map((c, n) => n === i ? { ...c, type } : c))} /><label className="form-check"><input type="checkbox" checked={column.nullable} onChange={(e) => setColumns(columns.map((c, n) => n === i ? { ...c, nullable: e.target.checked } : c))} /> nullable</label><button className="button button--icon" aria-label="Remove column" onClick={() => setColumns(columns.filter((_, n) => n !== i))}>×</button></div>)}<button className="button" onClick={() => setColumns([...columns, { name: '', type: 'text', nullable: true }])}>Add column</button></div>
      <label className="catalog-form__field">Primary key <span className="catalog-form__hint">comma-separated column names, in order</span><input value={primaryKey} onChange={(e) => setPrimaryKey(e.target.value)} /></label>
      <div className="catalog-form__section"><h2>Indexes</h2>{indexes.map((index, i) => <div className="catalog-form__row" key={i}><input aria-label="Index name" value={index.name} placeholder="index name" onChange={(e) => setIndexes(indexes.map((x, n) => n === i ? { ...x, name: e.target.value } : x))} /><input aria-label="Index columns" value={index.columns.join(', ')} placeholder="columns" onChange={(e) => setIndexes(indexes.map((x, n) => n === i ? { ...x, columns: splitList(e.target.value) } : x))} /><label className="form-check"><input type="checkbox" checked={Boolean(index.unique)} onChange={(e) => setIndexes(indexes.map((x, n) => n === i ? { ...x, unique: e.target.checked } : x))} /> unique</label><button className="button button--icon" aria-label="Remove index" onClick={() => setIndexes(indexes.filter((_, n) => n !== i))}>×</button></div>)}<button className="button" onClick={() => setIndexes([...indexes, { name: '', columns: [], unique: false }])}>Add index</button></div>
      <div className="catalog-form__section"><h2>Foreign keys</h2>{foreignKeys.map((key, i) => <div className="catalog-form__foreign-key-row" key={i}><input aria-label="Foreign key name" value={key.name} placeholder="constraint name" onChange={(e) => setForeignKeys(foreignKeys.map((x, n) => n === i ? { ...x, name: e.target.value } : x))} /><input aria-label="Referencing columns" value={key.columns.join(', ')} placeholder="local columns" onChange={(e) => setForeignKeys(foreignKeys.map((x, n) => n === i ? { ...x, columns: splitList(e.target.value) } : x))} /><input aria-label="Referenced table" value={key.ref_table} placeholder="referenced table" onChange={(e) => setForeignKeys(foreignKeys.map((x, n) => n === i ? { ...x, ref_table: e.target.value } : x))} /><input aria-label="Referenced columns" value={key.ref_columns.join(', ')} placeholder="referenced primary key" onChange={(e) => setForeignKeys(foreignKeys.map((x, n) => n === i ? { ...x, ref_columns: splitList(e.target.value) } : x))} /><button className="button button--icon" aria-label="Remove foreign key" onClick={() => setForeignKeys(foreignKeys.filter((_, n) => n !== i))}>×</button></div>)}<button className="button" onClick={() => setForeignKeys([...foreignKeys, { name: '', columns: [], ref_table: '', ref_columns: [] }])}>Add foreign key</button></div>
      <div className="catalog-form__footer"><span>Server-side validation is authoritative.</span><button className="button button--primary" disabled={submitting} onClick={create}>{submitting ? 'Creating…' : 'Create table'}</button></div>
    </div>
  </section>
}

type TabProps = { table: TableInfo; mutable: boolean; onChanged: (table: TableInfo) => void; setError: (message: string | null) => void }

function TypeSelect({ value, onChange }: { value: ColumnInfo['type']; onChange: (value: ColumnInfo['type']) => void }) { return <select value={value} onChange={(e) => onChange(e.target.value as ColumnInfo['type'])}>{columnTypes.map((type) => <option key={type}>{type}</option>)}</select> }
function splitList(value: string) { return value.split(',').map((item) => item.trim()).filter(Boolean) }
function displayValue(value: unknown) { return value === null || value === undefined ? 'NULL' : typeof value === 'object' ? JSON.stringify(value) : String(value) }
