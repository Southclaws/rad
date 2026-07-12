// Thin client for the rad REST API (same origin; Vite dev server proxies).

export interface KVEntry {
  key: string // base64
  keyDisplay: string
  valueSize: number
  valueDisplay: string
}

export interface KVScanResult {
  entries: KVEntry[]
  truncated: boolean
  nextAfter?: string
}

export interface KVDetail {
  key: string
  keyDisplay: string
  keyHex: string
  valueSize: number
  valueDisplay: string
  valueHex: string
  valueJSON?: unknown
}

export interface Column {
  name: string
  type: string
  nullable: boolean
  pk: boolean
}

export interface TableInfo {
  name: string
  id: string
  columns: Column[]
}

export interface CatalogIndex {
  id: string
  name: string
  columns: string[]
  unique: boolean
}

export interface CatalogFK {
  id: string
  name: string
  columns: string[]
  ref_table_id: string
  ref_columns: string[]
}

export interface CatalogColumn {
  id: string
  name: string
  type: string
  nullable: boolean
}

export interface CatalogTable {
  id: string
  schema_id: string
  name: string
  columns: CatalogColumn[]
  primary_key: string[]
  indexes: CatalogIndex[] | null
  foreign_keys: CatalogFK[] | null
}

export interface SchemaResult {
  schema: string
  tables: CatalogTable[]
}

export interface Cell {
  d: string
  null?: boolean
}

export interface RowResult {
  key?: string
  keyDisplay?: string
  cells: Cell[]
}

export interface RowsResult {
  table: TableInfo
  rows: RowResult[]
  truncated?: boolean
  nextAfter?: string
}

export interface Meta {
  schema: string
  dbPath: string
}

async function get<T>(url: string): Promise<T> {
  const res = await fetch(url)
  if (!res.ok) {
    let msg = `${res.status} ${res.statusText}`
    try {
      const body = await res.json()
      if (body.error) msg = body.error
    } catch {
      /* keep status text */
    }
    throw new Error(msg)
  }
  return res.json()
}

export const api = {
  meta: () => get<Meta>('/api/meta'),
  schema: () => get<SchemaResult>('/api/schema'),
  kvScan: (prefix: string, after?: string, limit = 100) => {
    const p = new URLSearchParams({ prefix, limit: String(limit) })
    if (after) p.set('after', after)
    return get<KVScanResult>(`/api/kv/scan?${p}`)
  },
  kvGet: (key64: string) => get<KVDetail>(`/api/kv/get?key=${encodeURIComponent(key64)}`),
  tableRows: (table: string, after?: string, limit = 100) => {
    const p = new URLSearchParams({ limit: String(limit) })
    if (after) p.set('after', after)
    return get<RowsResult>(`/api/tables/${encodeURIComponent(table)}/rows?${p}`)
  },
  indexScan: (table: string, index: string, values: string[]) => {
    const p = new URLSearchParams({ index })
    for (const v of values) p.append('v', v)
    return get<RowsResult>(`/api/tables/${encodeURIComponent(table)}/indexscan?${p}`)
  },
}
