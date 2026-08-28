// The driver seam between the protocol dispatcher and Playwright. The
// dispatcher tests inject a fake implementation so no browser launches.

import type { Params } from "./params.js";
import type {
	EndFlowParams,
	EndFlowResult,
	StartFlowParams,
	StepCommand,
} from "./protocol.js";

export interface ShimDriver {
	readonly playwrightVersion: string;
	startFlow(params: StartFlowParams): Promise<void>;
	endFlow(params: EndFlowParams): Promise<EndFlowResult>;
	cancelFlow(): Promise<void>;
	runStep(cmd: StepCommand, params: Params): Promise<Params>;
	dispose(): Promise<void>;
}
