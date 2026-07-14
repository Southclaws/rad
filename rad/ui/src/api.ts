// The admin surface has two deliberately separate clients. KV inspection is
// private to the admin port; catalog and query operations use the public API.

export interface KVEntry {
  key: string
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

export interface DatabaseInfo {
  mode: 'direct' | 'schema'
  location?: string
}

export interface ColumnDefault {
  func?: string
  value?: string | number | boolean
}

export interface ColumnInfo {
  name: string
  type: 'text' | 'int64' | 'float64' | 'bool'
  nullable?: boolean
  format?: string
  default?: ColumnDefault
}

export interface IndexInfo {
  name: string
  columns: string[]
  unique?: boolean
}

export interface ForeignKeyInfo {
  name: string
  columns: string[]
  ref_table: string
  ref_columns: string[]
}

export interface TableInfo {
  name: string
  columns: ColumnInfo[]
  primary_key: string[]
  indexes?: IndexInfo[]
  foreign_keys?: ForeignKeyInfo[]
}

export interface Health {
  status: string
  mode: 'direct' | 'schema'
}

const publicURL = new URL(window.location.href)
const adminPort = Number(publicURL.port)
publicURL.port = String(adminPort > 0 ? adminPort - 1 : 7237)
const publicBase = publicURL.origin

async function request<T>(url: string, init?: RequestInit): Promise<T> {
  const res = await fetch(url, init)
  if (!res.ok) {
    let message = `${res.status} ${res.statusText}`
    try {
      const body = await res.json()
      message = body.detail ?? body.error ?? message
    } catch {
      // Keep the HTTP status when the response is not JSON.
    }
    throw new Error(message)
  }
  if (res.status === 204) return undefined as T
  return res.json() as Promise<T>
}

function publicRequest<T>(path: string, init?: RequestInit) {
  return request<T>(`${publicBase}${path}`, init)
}

function json(method: string, body?: unknown): RequestInit {
  return {
    method,
    headers: { 'Content-Type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  }
}

export const adminAPI = {
  kvScan: (prefix: string, after?: string, limit = 100) => {
    const p = new URLSearchParams({ prefix, limit: String(limit) })
    if (after) p.set('after', after)
    return request<KVScanResult>(`/api/kv/scan?${p}`)
  },
  kvGet: (key64: string) => request<KVDetail>(`/api/kv/get?key=${encodeURIComponent(key64)}`),
  // The body is the raw LIR document as typed; the server validates it and
  // returns the rendered physical plan (or an error to surface).
  plan: (query: string) =>
    request<{ plan: string }>('/api/plan', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: query,
    }),
}

export const publicAPI = {
  info: () => publicRequest<DatabaseInfo>('/info'),
  health: () => publicRequest<Health>('/health'),
  tables: () => publicRequest<{ tables: TableInfo[] }>('/tables'),
  createTable: (table: TableInfo) => publicRequest<TableInfo>('/tables', json('POST', table)),
  renameTable: (table: string, name: string) =>
    publicRequest<TableInfo>(`/tables/${encodeURIComponent(table)}`, json('PATCH', { name })),
  deleteTable: (table: string) => publicRequest<void>(`/tables/${encodeURIComponent(table)}`, json('DELETE')),
  createColumn: (table: string, column: ColumnInfo) =>
    publicRequest<TableInfo>(`/tables/${encodeURIComponent(table)}/columns`, json('POST', column)),
  renameColumn: (table: string, column: string, name: string) =>
    publicRequest<TableInfo>(`/tables/${encodeURIComponent(table)}/columns/${encodeURIComponent(column)}`, json('PATCH', { name })),
  deleteColumn: (table: string, column: string) =>
    publicRequest<TableInfo>(`/tables/${encodeURIComponent(table)}/columns/${encodeURIComponent(column)}`, json('DELETE')),
  createIndex: (table: string, index: IndexInfo) =>
    publicRequest<TableInfo>(`/tables/${encodeURIComponent(table)}/indexes`, json('POST', index)),
  deleteIndex: (table: string, index: string) =>
    publicRequest<TableInfo>(`/tables/${encodeURIComponent(table)}/indexes/${encodeURIComponent(index)}`, json('DELETE')),
  query: (query: unknown) => publicRequest<{ result: unknown }>('/query', json('POST', query)),
}
