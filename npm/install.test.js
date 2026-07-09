"use strict";

const assert = require("node:assert/strict");
const crypto = require("node:crypto");
const test = require("node:test");

const { parseChecksum, verifyChecksum } = require("./install.js");

test("parses a standard sha256 checksum file", () => {
  const checksum = "a".repeat(64);
  assert.equal(parseChecksum(`${checksum}  rmux-darwin-arm64\n`), checksum);
});

test("rejects invalid checksum text", () => {
  assert.throws(() => parseChecksum("not-a-checksum"), /missing or invalid/);
});

test("verifies downloaded bytes", () => {
  const buffer = Buffer.from("rmux release asset");
  const checksum = crypto.createHash("sha256").update(buffer).digest("hex");

  assert.equal(verifyChecksum(buffer, checksum), checksum);
  assert.throws(
    () => verifyChecksum(buffer, "0".repeat(64)),
    /checksum mismatch/,
  );
});
