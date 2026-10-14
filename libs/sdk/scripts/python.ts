import { writeFileSync } from 'node:fs';
import { pythonUrlMap } from '../src/python.ts';

writeFileSync(new URL('../src/urls.py', import.meta.url), pythonUrlMap());
