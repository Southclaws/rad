import { useCallback, useEffect, useState } from 'react'
import { api, type KVDetail, type KVEntry } from './api'

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
    api
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
    api
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
    <div className="kv-browser">
      <div className="kv-controls">
        <div className="prefix-row">
          <input
            className="prefix-input"
            value={prefixInput}
            onChange={(e) => setPrefixInput(e.target.value)}
            onKeyDown={(e) => e.key === 'Enter' && go()}
            placeholder="key prefix…"
            spellCheck={false}
          />
          <button className="btn primary" onClick={go}>scan</button>
        </div>
        <div className="chips">
          {QUICK_PREFIXES.map((p) => (
            <button
              key={p}
              className={`chip ${prefix === p ? 'active' : ''}`}
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

      {error && <div className="error-banner">{error}</div>}

      <div className="kv-split">
        <div className="kv-list">
          <div className="kv-list-head">
            <span>{entries.length} keys{nextAfter ? '+' : ''}</span>
          </div>
          {entries.map((e) => (
            <button
              key={e.key}
              className={`kv-row ${selectedKey === e.key ? 'selected' : ''}`}
              onClick={() => select(e.key)}
            >
              <span className="kv-key">{e.keyDisplay}</span>
              <span className="kv-preview">{e.valueDisplay || '∅'}</span>
              <span className="kv-size">{e.valueSize}B</span>
            </button>
          ))}
          {entries.length === 0 && !loading && <div className="kv-empty">no keys under this prefix</div>}
          {nextAfter && (
            <button className="btn load-more" disabled={loading} onClick={() => load(prefix, nextAfter)}>
              {loading ? 'loading…' : 'load more'}
            </button>
          )}
        </div>

        {selected && (
          <div className="kv-detail">
            <h3>key</h3>
            <div className="mono block">{selected.keyDisplay}</div>
            <details>
              <summary>raw key ({selected.key.length}B base64)</summary>
              <pre className="hex">{selected.keyHex}</pre>
            </details>
            <h3>value <span className="dim">({selected.valueSize}B)</span></h3>
            {selected.valueJSON !== undefined ? (
              <pre className="json block">{JSON.stringify(selected.valueJSON, null, 2)}</pre>
            ) : (
              <div className="mono block">{selected.valueDisplay || '∅ (empty)'}</div>
            )}
            <details>
              <summary>raw value</summary>
              <pre className="hex">{selected.valueHex}</pre>
            </details>
          </div>
        )}
      </div>
    </div>
  )
}
