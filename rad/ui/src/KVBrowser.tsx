import { useCallback, useEffect, useState } from 'react'
import { adminAPI, type KVDetail, type KVEntry } from './api'

const QUICK_PREFIXES = ['/rad/', '/rad/catalog/', '/rad/data/', '/rad/index/']

export function KVBrowser({
  initialPrefix,
  initialSelectKey,
}: {
  initialPrefix: string
  initialSelectKey?: string
}) {
  const [prefixInput, setPrefixInput] = useState(initialPrefix)
  const [prefix, setPrefix] = useState(initialPrefix)
  const [entries, setEntries] = useState<KVEntry[]>([])
  const [nextAfter, setNextAfter] = useState<string | undefined>()
  const [selected, setSelected] = useState<KVDetail | null>(null)
  const [selectedKey, setSelectedKey] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(false)

  const load = useCallback((p: string, after?: string) => {
    setLoading(true)
    setError(null)
    adminAPI
      .kvScan(p, after)
      .then((res) => {
        setEntries((prev) => (after ? [...prev, ...res.entries] : res.entries))
        setNextAfter(res.truncated ? res.nextAfter : undefined)
      })
      .catch((e) => setError(String(e)))
      .finally(() => setLoading(false))
  }, [])

  useEffect(() => {
    load(prefix)
  }, [prefix, load])

  const select = useCallback((key64: string) => {
    setSelectedKey(key64)
    setSelected(null)
    adminAPI
      .kvGet(key64)
      .then(setSelected)
      .catch((e) => setError(String(e)))
  }, [])

  useEffect(() => {
    if (initialSelectKey) select(initialSelectKey)
  }, [initialSelectKey, select])

  const go = () => {
    setSelected(null)
    setSelectedKey(null)
    setPrefix(prefixInput)
  }

  return (
    <div className="kv-inspector">
      <div className="kv-inspector__controls">
        <div className="kv-inspector__prefix-row">
          <input
            className="kv-inspector__prefix-input"
            value={prefixInput}
            onChange={(e) => setPrefixInput(e.target.value)}
            onKeyDown={(e) => e.key === 'Enter' && go()}
            placeholder="key prefix…"
            spellCheck={false}
          />
          <button className="button button--primary" onClick={go}>scan</button>
        </div>
        <div className="kv-inspector__prefixes">
          {QUICK_PREFIXES.map((p) => (
            <button
              key={p}
              className={`kv-inspector__prefix ${prefix === p ? 'kv-inspector__prefix--active' : ''}`}
              onClick={() => {
                setPrefixInput(p)
                setSelected(null)
                setSelectedKey(null)
                setPrefix(p)
              }}
            >
              {p}
            </button>
          ))}
        </div>
      </div>

      {error && <div className="ui-notice ui-notice--error">{error}</div>}

      <div className="kv-inspector__split">
        <div className="kv-inspector__list">
          <div className="kv-inspector__list-head">
            <span>{entries.length} keys{nextAfter ? '+' : ''}</span>
          </div>
          {entries.map((e) => (
            <button
              key={e.key}
              className={`kv-inspector__row ${selectedKey === e.key ? 'kv-inspector__row--selected' : ''}`}
              onClick={() => select(e.key)}
            >
              <span className="kv-inspector__key">{e.keyDisplay}</span>
              <span className="kv-inspector__preview">{e.valueDisplay || '∅'}</span>
              <span className="kv-inspector__size">{e.valueSize}B</span>
            </button>
          ))}
          {entries.length === 0 && !loading && <div className="kv-inspector__empty">no keys under this prefix</div>}
          {nextAfter && (
            <button className="button kv-inspector__load-more" disabled={loading} onClick={() => load(prefix, nextAfter)}>
              {loading ? 'loading…' : 'load more'}
            </button>
          )}
        </div>

        {selected && (
          <div className="kv-inspector__detail">
            <h3>key</h3>
            <div className="code-block">{selected.keyDisplay}</div>
            <details>
              <summary>raw key ({selected.key.length}B base64)</summary>
              <pre className="code-block code-block--hex">{selected.keyHex}</pre>
            </details>
            <h3>value <span className="text-muted">({selected.valueSize}B)</span></h3>
            {selected.valueJSON !== undefined ? (
              <pre className="code-block code-block--json">{JSON.stringify(selected.valueJSON, null, 2)}</pre>
            ) : (
              <div className="code-block">{selected.valueDisplay || '∅ (empty)'}</div>
            )}
            <details>
              <summary>raw value</summary>
              <pre className="code-block code-block--hex">{selected.valueHex}</pre>
            </details>
          </div>
        )}
      </div>
    </div>
  )
}
