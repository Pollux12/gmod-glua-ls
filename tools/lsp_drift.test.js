'use strict';

const assert = require('node:assert/strict');
const { test } = require('node:test');
const { settlePush } = require('./lsp_drift');

test('push diagnostics wait for a deletion to finish even when publishes are quiet', async () => {
    let shown = [];
    let finishDeletion;
    const reindexed = new Promise((resolve) => {
        finishDeletion = () => {
            shown = ['undefined-field', 'unchecked-nil-access'];
            resolve({ message: { result: [] } });
        };
    });
    const client = {
        statusAt: new Map([['startupComplete', 0]]),
        request: (method) => {
            assert.equal(method, 'workspace/symbol');
            return reindexed;
        },
        waitForPublishQuiet: async () => {},
    };
    const settled = settlePush(client, 1);
    setImmediate(finishDeletion);
    await settled;
    assert.deepEqual(shown, ['undefined-field', 'unchecked-nil-access']);
});
