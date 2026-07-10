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
    console.error(
      [
        "rmux native binary is missing because its install script did not run.",
        "Bun: bun pm -g trust @cookedoss/rmux",
        "npm: npm install -g @cookedoss/rmux",
      ].join("\n"),
    );
  } else {
    console.error(`rmux failed to start: ${result.error.message}`);
  }
  process.exit(1);
}

process.exit(result.status ?? 0);
