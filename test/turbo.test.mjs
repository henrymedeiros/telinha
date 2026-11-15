import test from "node:test";
import assert from "node:assert/strict";
import { sharedCodec, enabledCodecs } from "../turbo.js";

test("a codec is shared only when every viewer lists it", () => {
  assert.equal(sharedCodec([["hevc", "av1"]], ["av1", "hevc"]), "av1");
  assert.equal(sharedCodec([["hevc", "av1"], ["hevc"]], ["av1", "hevc"]), "hevc");
  assert.equal(sharedCodec([["av1"], ["hevc"]], ["av1", "hevc"]), null);
  assert.equal(sharedCodec([["hevc"], []], ["av1", "hevc"]), null);
  assert.equal(sharedCodec([], ["av1", "hevc"]), null);
  assert.equal(sharedCodec([["hevc"]], []), null);
});

test("?enc= turns the extra codecs on, in the sender's own order", () => {
  assert.deepEqual(enabledCodecs(""), []);
  assert.deepEqual(enabledCodecs("?enc=hevc"), ["hevc"]);
  assert.deepEqual(enabledCodecs("?enc=hevc,av1"), ["av1", "hevc"]);
  assert.deepEqual(enabledCodecs("?enc=vp9,,x"), []);
  assert.deepEqual(enabledCodecs(undefined), []);
});
