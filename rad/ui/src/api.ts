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

// The query plan for one statement: the structured view and its rendered text.
export interface StatementPlan {
  name: string
  view: unknown // the structured PlanView (op/detail/access/children); free-form
  text: string // the pretty rendered tree, with access decisions
}

export interface ExecuteResponse {
  result: unknown
  statements: { name: string; affected: number }[]
  // Free-form: present as { statements: StatementPlan[] } when show-plan was
  // set, otherwise null.
  plan: { statements: StatementPlan[] } | null
}

export const adminAPI = {
  kvScan: (prefix: string, after?: string, limit = 100) => {
    const p = new URLSearchParams({ prefix, limit: String(limit) })
    if (after) p.set('after', after)
    return request<KVScanResult>(`/api/kv/scan?${p}`)
  },
  kvGet: (key64: string) => request<KVDetail>(`/api/kv/get?key=${encodeURIComponent(key64)}`),
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
  // Run a PIR program. show-plan attaches the query plan for each statement;
  // dry-run binds and plans but executes nothing (no result).
  execute: (program: unknown, opts?: { showPlan?: boolean; dryRun?: boolean }) => {
    const p = new URLSearchParams()
    if (opts?.showPlan) p.set('show-plan', 'true')
    if (opts?.dryRun) p.set('dry-run', 'true')
    const qs = p.toString()
    return publicRequest<ExecuteResponse>(`/execute${qs ? `?${qs}` : ''}`, json('POST', program))
  },
}
