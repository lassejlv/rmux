#!/usr/bin/env node
"use strict";

const path = require("path");
const { spawnSync } = require("child_process");

const binary = path.join(__dirname, "rmux-native");

const result = spawnSync(binary, process.argv.slice(2), {
  stdio: "inherit",
  argv0: "rmux",
});

if (result.error) {
  if (result.error.code === "ENOENT") {
    console.error("rmux binary is missing. Reinstall the package: npm install -g @lassejlv/rmux");
  } else {
    console.error(`rmux failed to start: ${result.error.message}`);
  }
  process.exit(1);
}

process.exit(result.status ?? 0);
