import { useCallback, useEffect, useState } from 'react'
import { publicAPI, type DatabaseInfo, type TableInfo } from './api'
import { KVBrowser } from './KVBrowser'
import { PlanView } from './PlanView'
import { NewTableView, TableView } from './TableView'

export type View = { kind: 'kv'; prefix?: string } | { kind: 'plan' } | { kind: 'table'; name: string } | { kind: 'new-table' }

export default function App() {
  const [view, setView] = useState<View>({ kind: 'kv', prefix: '/rad/' })
  const [info, setInfo] = useState<DatabaseInfo | null>(null)
  const [tables, setTables] = useState<TableInfo[]>([])
  const [error, setError] = useState<string | null>(null)

  const refreshTables = useCallback(() => {
    publicAPI.tables().then(({ tables }) => setTables(tables)).catch((e) => setError(String(e)))
  }, [])

  useEffect(() => {
    Promise.all([publicAPI.info(), publicAPI.tables()]).then(([databaseInfo, result]) => {
      setInfo(databaseInfo)
      setTables(result.tables)
    }).catch((e) => setError(String(e)))
  }, [])

  const selectedTable = view.kind === 'table' ? tables.find((table) => table.name === view.name) : undefined

  return (
    <div className="admin-shell">
      <header className="admin-shell__header">
        <span className="admin-shell__brand">rad</span>
        <span className="admin-shell__product">admin</span>
        {info && <span className={`admin-shell__mode admin-shell__mode--${info.mode}`}>{info.mode} catalog</span>}
        {info?.location && <span className="admin-shell__database" title={info.location}>{info.location}</span>}
      </header>
      <div className="admin-shell__body">
        <nav className="admin-nav" aria-label="Admin navigation">
          <button className={`admin-nav__item ${view.kind === 'kv' ? 'admin-nav__item--active' : ''}`} onClick={() => setView({ kind: 'kv', prefix: '/rad/' })}>
            <span className="admin-nav__icon">⌘</span> KV inspector
          </button>
          <button className={`admin-nav__item ${view.kind === 'plan' ? 'admin-nav__item--active' : ''}`} onClick={() => setView({ kind: 'plan' })}>
            <span className="admin-nav__icon">⋔</span> Query plan
          </button>
          <div className="admin-nav__section">
            <span>tables</span>
            <button className="admin-nav__refresh" title="Refresh tables" onClick={refreshTables}>↻</button>
          </div>
          <button className={`admin-nav__item admin-nav__item--create ${view.kind === 'new-table' ? 'admin-nav__item--active' : ''}`} onClick={() => setView({ kind: 'new-table' })}>
            <span className="admin-nav__icon">+</span> Create table
          </button>
          {tables.map((table) => (
            <button key={table.name} className={`admin-nav__item ${view.kind === 'table' && view.name === table.name ? 'admin-nav__item--active' : ''}`} onClick={() => setView({ kind: 'table', name: table.name })}>
              <span className="admin-nav__icon">▤</span> {table.name}
            </button>
          ))}
          {info?.mode === 'schema' && <p className="admin-nav__note">Schema-managed: catalog edits are unavailable.</p>}
          {tables.length === 0 && <p className="admin-nav__empty">No tables yet. Create one from the public catalog API.</p>}
        </nav>
        <main className="admin-shell__content">
          {error && <div className="ui-notice ui-notice--error" role="alert">{error}</div>}
          {view.kind === 'kv' && <KVBrowser initialPrefix={view.prefix ?? '/rad/'} />}
          {view.kind === 'plan' && <PlanView />}
          {view.kind === 'new-table' && (
            <NewTableView
              onCreated={(table) => {
                refreshTables()
                setView({ kind: 'table', name: table.name })
              }}
            />
          )}
          {view.kind === 'table' && selectedTable && (
            <TableView
              key={selectedTable.name}
              table={selectedTable}
              mutable={info?.mode === 'direct'}
              onChanged={(table) => {
                refreshTables()
                setView({ kind: 'table', name: table.name })
              }}
              onDeleted={() => {
                refreshTables()
                setView({ kind: 'new-table' })
              }}
            />
          )}
          {view.kind === 'table' && !selectedTable && <div className="empty-state">That table no longer exists. Refreshing the catalog…</div>}
        </main>
      </div>
    </div>
  )
}
