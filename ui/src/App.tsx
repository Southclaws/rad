import { useEffect, useState } from 'react'
import { api, type Meta, type SchemaResult } from './api'
import { KVBrowser } from './KVBrowser'
import { TableView } from './TableView'

export type View =
  | { kind: 'kv'; prefix?: string; selectKey?: string }
  | { kind: 'table'; name: string }

export default function App() {
  const [view, setView] = useState<View>({ kind: 'kv', prefix: '/rad/' })
  const [meta, setMeta] = useState<Meta | null>(null)
  const [schema, setSchema] = useState<SchemaResult | null>(null)
  const [error, setError] = useState<string | null>(null)

  const refreshSchema = () => {
    api.schema().then(setSchema).catch((e) => setError(String(e)))
  }

  useEffect(() => {
    api.meta().then(setMeta).catch((e) => setError(String(e)))
    refreshSchema()
  }, [])

  return (
    <div className="app">
      <header className="topbar">
        <span className="logo">rad</span>
        <span className="topbar-sub">relational-over-KV devtool</span>
        {meta && <span className="topbar-db" title={meta.dbPath}>{meta.dbPath}</span>}
      </header>
      <div className="body">
        <nav className="sidebar">
          <button
            className={`nav-item ${view.kind === 'kv' ? 'active' : ''}`}
            onClick={() => setView({ kind: 'kv', prefix: '/rad/' })}
          >
            <span className="nav-icon">⌘</span> KV Browser
          </button>
          <div className="nav-section">
            tables
            <button className="nav-refresh" title="Refresh tables" onClick={refreshSchema}>↻</button>
          </div>
          {schema?.tables.map((t) => (
            <button
              key={t.id}
              className={`nav-item nav-table ${view.kind === 'table' && view.name === t.name ? 'active' : ''}`}
              onClick={() => setView({ kind: 'table', name: t.name })}
            >
              <span className="nav-icon">▤</span> {t.name}
              <span className="nav-id">{t.id}</span>
            </button>
          ))}
          {schema && schema.tables.length === 0 && (
            <div className="nav-empty">no tables — seed with `task demo`</div>
          )}
        </nav>
        <main className="content">
          {error && <div className="error-banner">{error}</div>}
          {view.kind === 'kv' && (
            <KVBrowser
              key={`${view.prefix}|${view.selectKey ?? ''}`}
              initialPrefix={view.prefix ?? '/rad/'}
              initialSelectKey={view.selectKey}
            />
          )}
          {view.kind === 'table' && schema && (
            <TableView
              key={view.name}
              name={view.name}
              def={schema.tables.find((t) => t.name === view.name)}
              onBrowseKeys={(prefix, selectKey) => setView({ kind: 'kv', prefix, selectKey })}
            />
          )}
        </main>
      </div>
    </div>
  )
}
