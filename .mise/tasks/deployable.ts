#!/usr/bin/env node
//MISE description="The images, the declarations alone, or the Workers a change needs, as JSON: `deployable [--workers | --declared] [--all] [--since REV | --git BASE [--head REV]] [--files ...]`"
/**
 * What a change needs built: the workspace's engine, over this repository's images and Workers.
 * CI reads the list to decide what to build; see infra's spec/architecture/host.md, "The machine
 * pulls; nothing pushes into it", and spec/architecture/deployer.md, "What a Worker artifact is".
 */
import type * as Deployable from '../../../../.mise/tasks/change/deployable.ts';
import { LAYOUT, shared } from './workspace.ts';

const { main } = (await import(shared('change/deployable.ts'))) as typeof Deployable;

export const CONFIG: Deployable.Config = { layout: LAYOUT, images: true, workers: {} };

if (import.meta.main) main(CONFIG);
