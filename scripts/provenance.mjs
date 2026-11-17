#!/usr/bin/env node
// usage: provenance.mjs [<out.json>]

import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";

import { collectProvenance } from "./lib/provenance.mjs";

const out = resolve(
  process.argv[2] ??
    join(import.meta.dirname, "..", "target", "provenance.json"),
);
const record = collectProvenance();
mkdirSync(dirname(out), { recursive: true });
writeFileSync(out, `${JSON.stringify(record, null, 2)}\n`);
for (const [key, value] of Object.entries(record))
  for (const line of Array.isArray(value) ? value : [value])
    process.stdout.write(`${key}: ${line}\n`);
