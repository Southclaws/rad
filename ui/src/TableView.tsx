import { useCallback, useEffect, useState } from 'react'
import { api, type CatalogTable, type RowResult, type RowsResult } from './api'

type Tab = 'data' | 'structure' | 'keys'

export function TableView({
  name,
  def,
  onBrowseKeys,
}: {
  name: string
  def?: CatalogTable
  onBrowseKeys: (prefix: string, selectKey?: string) => void
}) {
  const [tab, setTab] = useState<Tab>('data')

  return (
    <div className="table-view">
      <div className="table-head">
        <h2>
          {name} {def && <span className="dim">{def.id}</span>}
        </h2>
        <div className="tabs">
          {(['data', 'structure', 'keys'] as Tab[]).map((t) => (
            <button key={t} className={`tab ${tab === t ? 'active' : ''}`} onClick={() => setTab(t)}>
              {t}
            </button>
          ))}
        </div>
      </div>
      {tab === 'data' && <DataTab name={name} def={def} onBrowseKeys={onBrowseKeys} />}
      {tab === 'structure' && def && <StructureTab def={def} />}
      {tab === 'keys' && def && <KeysTab def={def} onBrowseKeys={onBrowseKeys} />}
    </div>
  )
}

function DataTab({
  name,
  def,
  onBrowseKeys,
}: {
  name: string
  def?: CatalogTable
  onBrowseKeys: (prefix: string, selectKey?: string) => void
}) {
  const [result, setResult] = useState<RowsResult | null>(null)
  const [rows, setRows] = useState<RowResult[]>([])
  const [nextAfter, setNextAfter] = useState<string | undefined>()
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(false)

  // Index filter state.
  const indexes = def?.indexes ?? []
  const [filterIndex, setFilterIndex] = useState('')
  const [filterValues, setFilterValues] = useState<string[]>([])
  const activeIndex = indexes.find((i) => i.name === filterIndex)

  const loadScan = useCallback(
    (after?: string) => {
      setLoading(true)
      setError(null)
      api
        .tableRows(name, after)
        .then((res) => {
          setResult(res)
          setRows((prev) => (after ? [...prev, ...res.rows] : res.rows))
          setNextAfter(res.truncated ? res.nextAfter : undefined)
        })
        .catch((e) => setError(String(e)))
        .finally(() => setLoading(false))
    },
    [name],
  )

  useEffect(() => {
    loadScan()
  }, [loadScan])

  const runIndexScan = () => {
    if (!activeIndex) return
    setLoading(true)
    setError(null)
    const vals = filterValues.filter((v) => v !== '')
    api
      .indexScan(name, activeIndex.name, vals)
      .then((res) => {
        setResult(res)
        setRows(res.rows)
        setNextAfter(undefined)
      })
      .catch((e) => setError(String(e)))
      .finally(() => setLoading(false))
  }

  const clearFilter = () => {
    setFilterIndex('')
    setFilterValues([])
    loadScan()
  }

  return (
    <div>
      {indexes.length > 0 && (
        <div className="filter-bar">
          <select
            value={filterIndex}
            onChange={(e) => {
              setFilterIndex(e.target.value)
              setFilterValues([])
            }}
          >
            <option value="">— filter via index —</option>
            {indexes.map((i) => (
              <option key={i.id} value={i.name}>
                {i.name} ({i.columns.join(', ')}){i.unique ? ' unique' : ''}
              </option>
            ))}
          </select>
          {activeIndex?.columns.map((col, i) => (
            <input
              key={col}
              placeholder={col}
              value={filterValues[i] ?? ''}
              onChange={(e) => {
                const next = [...filterValues]
                next[i] = e.target.value
                setFilterValues(next)
              }}
              onKeyDown={(e) => e.key === 'Enter' && runIndexScan()}
            />
          ))}
          {activeIndex && (
            <button className="btn primary" onClick={runIndexScan}>scan index</button>
          )}
          {activeIndex && (
            <button className="btn" onClick={clearFilter}>clear</button>
          )}
        </div>
      )}

      {error && <div className="error-banner">{error}</div>}

      {result && (
        <div className="grid-wrap">
          <table className="grid">
            <thead>
              <tr>
                {result.table.columns.map((c) => (
                  <th key={c.name}>
                    {c.name}
                    {c.pk && <span className="badge pk">PK</span>}
                    <span className="col-type">{c.type}{c.nullable ? '?' : ''}</span>
                  </th>
                ))}
                <th className="key-col" />
              </tr>
            </thead>
            <tbody>
              {rows.map((r, i) => (
                <tr key={r.key ?? i}>
                  {r.cells.map((c, j) => (
                    <td key={j} className={c.null ? 'null' : ''}>
                      {c.null ? 'NULL' : c.d}
                    </td>
                  ))}
                  <td className="key-col">
                    {r.key && (
                      <button
                        className="key-link"
                        title={r.keyDisplay}
                        onClick={() => onBrowseKeys(rowPrefix(r.keyDisplay), r.key)}
                      >
                        ⌘
                      </button>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
          {rows.length === 0 && !loading && <div className="kv-empty">no rows</div>}
          {nextAfter && (
            <button className="btn load-more" disabled={loading} onClick={() => loadScan(nextAfter)}>
              {loading ? 'loading…' : 'load more'}
            </button>
          )}
          <div className="grid-foot">{rows.length} rows loaded{nextAfter ? ' (more available)' : ''}</div>
        </div>
      )}
    </div>
  )
}

// Extract the raw /rad/data/{t}/primary/ prefix from a decoded key display
// like "/rad/data/t2[users]/primary/(1)".
function rowPrefix(display?: string): string {
  if (!display) return '/rad/data/'
  const m = display.match(/^\/rad\/data\/([^[/]+)(\[[^\]]*\])?\/primary\//)
  return m ? `/rad/data/${m[1]}/primary/` : '/rad/data/'
}

function StructureTab({ def }: { def: CatalogTable }) {
  const pk = new Set(def.primary_key)
  return (
    <div className="structure">
      <h3>columns</h3>
      <table className="grid">
        <thead>
          <tr><th>id</th><th>name</th><th>type</th><th>nullable</th><th>primary key</th></tr>
        </thead>
        <tbody>
          {def.columns.map((c) => (
            <tr key={c.id}>
              <td className="dim">{c.id}</td>
              <td>{c.name}</td>
              <td>{c.type}</td>
              <td>{c.nullable ? 'yes' : 'no'}</td>
              <td>{pk.has(c.name) ? '✓' : ''}</td>
            </tr>
          ))}
        </tbody>
      </table>

      <h3>indexes</h3>
      {(def.indexes ?? []).length === 0 ? (
        <div className="dim">none</div>
      ) : (
        <table className="grid">
          <thead>
            <tr><th>id</th><th>name</th><th>columns</th><th>unique</th></tr>
          </thead>
          <tbody>
            {(def.indexes ?? []).map((i) => (
              <tr key={i.id}>
                <td className="dim">{i.id}</td>
                <td>{i.name}</td>
                <td>{i.columns.join(', ')}</td>
                <td>{i.unique ? 'yes' : 'no'}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}

      <h3>foreign keys</h3>
      {(def.foreign_keys ?? []).length === 0 ? (
        <div className="dim">none</div>
      ) : (
        <table className="grid">
          <thead>
            <tr><th>id</th><th>name</th><th>columns</th><th>references</th></tr>
          </thead>
          <tbody>
            {(def.foreign_keys ?? []).map((f) => (
              <tr key={f.id}>
                <td className="dim">{f.id}</td>
                <td>{f.name}</td>
                <td>{f.columns.join(', ')}</td>
                <td>
                  {f.ref_table_id}({f.ref_columns.join(', ')})
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  )
}

function KeysTab({
  def,
  onBrowseKeys,
}: {
  def: CatalogTable
  onBrowseKeys: (prefix: string) => void
}) {
  return (
    <div className="keys-tab">
      <p className="dim">
        Underlying KV namespaces for this table — click to open in the KV browser.
      </p>
      <button className="kv-row" onClick={() => onBrowseKeys(`/rad/data/${def.id}/primary/`)}>
        <span className="kv-key">/rad/data/{def.id}/primary/</span>
        <span className="kv-preview">primary rows (JSON values)</span>
      </button>
      {(def.indexes ?? []).map((i) => (
        <button
          key={i.id}
          className="kv-row"
          onClick={() => onBrowseKeys(`/rad/index/${def.id}/${i.id}/`)}
        >
          <span className="kv-key">/rad/index/{def.id}/{i.id}/</span>
          <span className="kv-preview">
            {i.name} — ({i.columns.join(', ')})+(pk){i.unique ? ', unique' : ''}
          </span>
        </button>
      ))}
      <button className="kv-row" onClick={() => onBrowseKeys(`/rad/catalog/table/${def.id}`)}>
        <span className="kv-key">/rad/catalog/table/{def.id}</span>
        <span className="kv-preview">catalog entry (JSON table definition)</span>
      </button>
    </div>
  )
}
