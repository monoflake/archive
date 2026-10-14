#!/usr/bin/env node
//MISE description="Refuse an app whose service.toml names it otherwise than its directory"
/**
 * The workspace's gate, over this repository's apps: the artifact is named for the directory and
 * the container for `name`, so the two are one. See RFC l6r4u.
 */
import type * as Names from '../../../../.mise/tasks/change/names.ts';
import { LAYOUT, shared } from './workspace.ts';

const { main } = (await import(shared('change/names.ts'))) as typeof Names;

main(LAYOUT);
