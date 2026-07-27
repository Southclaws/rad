// A decorative filler for the feature grid's odd cell on 3-column layouts:
// a one-to-many relational schematic (crow's-foot notation), drawn in
// phosphor hairlines. Non-informational; hidden below the 3-column breakpoint.
export function RelationGlyph() {
  return (
    <div className="deco" aria-hidden="true">
      <svg
        className="deco__svg"
        viewBox="0 0 220 132"
        fill="none"
        xmlns="http://www.w3.org/2000/svg"
      >
        {/* left table — the "one" */}
        <g stroke="var(--line-2)" strokeWidth="1.4">
          <rect x="18" y="28" width="70" height="78" rx="5" />
          <line x1="18" y1="47" x2="88" y2="47" />
          <line x1="31" y1="64" x2="75" y2="64" />
          <line x1="31" y1="79" x2="75" y2="79" />
          <line x1="31" y1="94" x2="75" y2="94" />
        </g>

        {/* right table — the "many" */}
        <g stroke="var(--line-2)" strokeWidth="1.4">
          <rect x="132" y="28" width="70" height="78" rx="5" />
          <line x1="132" y1="47" x2="202" y2="47" />
          <line x1="145" y1="64" x2="189" y2="64" />
          <line x1="145" y1="79" x2="189" y2="79" />
          <line x1="145" y1="94" x2="189" y2="94" />
        </g>

        {/* the relationship: one ──< many */}
        <g stroke="var(--green-mid)" strokeWidth="1.7" strokeLinecap="round">
          <line x1="88" y1="67" x2="120" y2="67" />
          <line x1="100" y1="58" x2="100" y2="76" />
          <line x1="120" y1="67" x2="132" y2="53" />
          <line x1="120" y1="67" x2="132" y2="67" />
          <line x1="120" y1="67" x2="132" y2="81" />
        </g>
        <circle className="deco__node" cx="88" cy="67" r="3.4" fill="var(--green)" />
      </svg>
      <span className="deco__label">one → many</span>
    </div>
  );
}
