import type { ReactNode } from "react";
import { IRDiagram, StackDiagram, ToolchainDiagram } from "./blog-diagrams";
import {
  AbstractionCurve,
  ClauseOrder,
  CompilerPipeline,
  LLVMHourglass,
  SharedIR,
} from "./relational-ir-diagrams";

export const blogDiagrams = {
  ToolchainDiagram,
  StackDiagram,
  IRDiagram,
  CompilerPipeline,
  LLVMHourglass,
  ClauseOrder,
  SharedIR,
  AbstractionCurve,
};

export function Callout({ children }: { children: ReactNode }) {
  return (
    <div className="callout" role="note">
      <span className="callout__tag" aria-hidden="true">
        note
      </span>
      <div>{children}</div>
    </div>
  );
}
