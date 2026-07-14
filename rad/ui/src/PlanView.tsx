import { useEffect, useState } from 'react'
import { adminAPI } from './api'

// The plan viewer renders the physical plan for a LIR query as you type. It is
// forgiving by design: a malformed or invalid query leaves the last good plan
// on screen and shows the error beneath, so editing never blanks the output.
export function PlanView() {
  const [text, setText] = useState('')
  const [plan, setPlan] = useState<string | null>(null) // last valid plan
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    const body = text.trim()
    if (!body) {
      setPlan(null)
      setError(null)
      return
    }
    // Debounce, and ignore a response if the text has moved on (latest wins).
    let live = true
    const timer = setTimeout(() => {
      adminAPI
        .plan(body)
        .then((res) => {
          if (!live) return
          setPlan(res.plan)
          setError(null)
        })
        .catch((e) => {
          if (live) setError(String(e))
        })
    }, 250)
    return () => {
      live = false
      clearTimeout(timer)
    }
  }, [text])

  return (
    <div className="plan-view">
      <div className="plan-view__split">
        <div className="plan-view__pane">
          <div className="plan-view__label">LIR query</div>
          <textarea
            className="plan-view__input"
            value={text}
            onChange={(e) => setText(e.target.value)}
            spellCheck={false}
            placeholder={'Type a LIR query as JSON…\n\n{\n  "nodes": { … },\n  "root": { "node": …, "cardinality": "many" }\n}'}
          />
        </div>
        <div className="plan-view__pane">
          <div className="plan-view__label">
            plan
            {error && plan !== null && <span className="plan-view__stale">stale</span>}
          </div>
          {plan !== null ? (
            <pre className="code-block plan-view__output">{plan}</pre>
          ) : (
            <div className="empty-state">A plan appears here as you type a valid query.</div>
          )}
        </div>
      </div>
      {error && <div className="ui-notice ui-notice--error">{error}</div>}
    </div>
  )
}
