import { describe, expect, it } from 'vitest';

import { activationDecisions, formatDate, resyncDecisions, statusTone } from './main.jsx';

describe('dashboard decisions', () => {
  it('keeps MyBrewFolio preselected only for differing activation Notes', () => {
    expect(activationDecisions({ items: [
      { sourceKey: 'one', differs: true },
      { sourceKey: 'two', differs: false },
    ] })).toEqual({ one: 'mybrewfolio' });
  });

  it('creates the safe default resync decisions', () => {
    expect(resyncDecisions({
      restoreItems: [{ id: 'restore-one' }],
      duplicates: [{ mapping_id: 'mapping', keep_shot_id: 'old', remove_shot_id: 'new', notes_conflict: true }],
    })).toEqual({
      restoreIds: ['restore-one'],
      duplicateDecisions: [{ mappingId: 'mapping', keepShotId: 'old', removeShotId: 'new', selected: true, notesResolution: '' }],
    });
  });

  it('uses deterministic status priority', () => {
    expect(statusTone('sync', 'Saved', 'success', 'Error')).toBe('working');
    expect(statusTone('', 'Saved', 'success', 'Error')).toBe('success');
    expect(statusTone('', '', 'success', 'Error')).toBe('error');
    expect(statusTone('', '', 'success', '')).toBe('info');
  });

  it('formats missing and invalid dates safely', () => {
    expect(formatDate(null)).toBe('Not synced yet');
    expect(formatDate('not-a-date')).toBe('Not synced yet');
  });
});
