import { useState } from 'react'
import { publicAPI, queryProgram, type ExecuteResponse } from './api'

// The query tool runs a LIR query or a PIR program through POST /execute,
// always asking for the query plan. The response shape maps cleanly to tabs:
// the result datum, the per-statement plan (physical operators + the planner's
// access-path decisions), and any error. "Dry run" binds and plans without
// executing — a safe way to inspect the plan of a mutating program.
type Tab = 'result' | 'plan' | 'errors'

export function QueryTool() {
  const [text, setText] = useState('')
  const [dryRun, setDryRun] = useState(false)
  const [running, setRunning] = useState(false)
  const [tab, setTab] = useState<Tab>('result')
  const [res, setRes] = useState<ExecuteResponse | null>(null)
  const [error, setError] = useState<string | null>(null)

  async function run() {
    let program: unknown
    try {
      const parsed = JSON.parse(text)
      // A bare LIR query is wrapped as a one-statement query program; a value
      // that already has `statements` is passed through as a full program.
      program =
        parsed && typeof parsed === 'object' && 'statements' in parsed
          ? parsed
          : queryProgram(parsed)
    } catch (e) {
      setError(`Invalid JSON: ${String(e)}`)
      setRes(null)
      setTab('errors')
      return
    }
    setRunning(true)
    try {
      const out = await publicAPI.execute(program, { showPlan: true, dryRun })
      setRes(out)
      setError(null)
      setTab(dryRun ? 'plan' : 'result')
    } catch (e) {
      setError(String(e))
      setRes(null)
      setTab('errors')
    } finally {
      setRunning(false)
    }
  }

  return (
    <div className="query-tool">
      <div className="query-tool__bar">
        <button className="query-tool__run" onClick={run} disabled={running || !text.trim()}>
          {running ? 'Running…' : 'Run ▸'}
        </button>
        <label className="query-tool__toggle">
          <input type="checkbox" checked={dryRun} onChange={(e) => setDryRun(e.target.checked)} />
          dry run — plan only, no execution
        </label>
      </div>
      <div className="query-tool__split">
        <div className="query-tool__pane">
          <div className="query-tool__label">LIR query or PIR program</div>
          <textarea
            className="query-tool__input"
            value={text}
            onChange={(e) => setText(e.target.value)}
            spellCheck={false}
            placeholder={
              'A LIR query…\n\n{\n  "nodes": { … },\n  "root": { "node": …, "cardinality": "many" }\n}\n\n…or a full PIR program: { "statements": [ … ] }'
            }
          />
        </div>
        <div className="query-tool__pane">
          <div className="query-tool__tabs">
            <TabButton id="result" tab={tab} setTab={setTab}>result</TabButton>
            <TabButton id="plan" tab={tab} setTab={setTab}>plan</TabButton>
            <TabButton id="errors" tab={tab} setTab={setTab} alert={error !== null}>errors</TabButton>
          </div>
          <div className="query-tool__panel">
            {tab === 'result' && <ResultPanel res={res} dryRun={dryRun} />}
            {tab === 'plan' && <PlanPanel res={res} />}
            {tab === 'errors' && <ErrorsPanel error={error} />}
          </div>
        </div>
      </div>
    </div>
  )
}

function TabButton(props: { id: Tab; tab: Tab; setTab: (t: Tab) => void; alert?: boolean; children: string }) {
  const active = props.tab === props.id
  return (
    <button
      className={`query-tool__tab ${active ? 'query-tool__tab--active' : ''}`}
      onClick={() => props.setTab(props.id)}
    >
      {props.children}
      {props.alert && <span className="query-tool__dot" aria-label="has errors" />}
    </button>
  )
}

function ResultPanel({ res, dryRun }: { res: ExecuteResponse | null; dryRun: boolean }) {
  if (!res) return <div className="empty-state">Run a query to see its result.</div>
  if (dryRun) return <div className="empty-state">Dry run: nothing was executed. See the plan tab.</div>
  return <pre className="code-block code-block--json query-tool__output">{JSON.stringify(res.result, null, 2)}</pre>
}

function PlanPanel({ res }: { res: ExecuteResponse | null }) {
  const stmts = res?.plan?.statements
  if (!stmts || stmts.length === 0) return <div className="empty-state">Run a query to see its plan.</div>
  return (
    <div className="query-tool__output">
      {stmts.map((s) => (
        <div key={s.name} className="query-tool__stmt">
          {stmts.length > 1 && <div className="query-tool__label">statement {s.name}</div>}
          <pre className="code-block query-tool__plan">{s.text}</pre>
        </div>
      ))}
    </div>
  )
}

function ErrorsPanel({ error }: { error: string | null }) {
  if (!error) return <div className="empty-state">No errors.</div>
  return <div className="ui-notice ui-notice--error query-tool__output">{error}</div>
}
