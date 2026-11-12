import { describe, expect, it } from 'vitest';
import { mergeSkillMenu, submenuPlacement } from './skillMenu';

describe('submenuPlacement', () => {
  it('opens downward when there is more room below the row', () => {
    expect(submenuPlacement(140, 175, 713)).toEqual({
      direction: 'down',
      maxHeight: Math.min(713 - 140 - 8, 713 * 0.6),
    });
  });

  it('opens upward when there is more room above the row', () => {
    expect(submenuPlacement(600, 635, 713)).toEqual({
      direction: 'up',
      maxHeight: Math.min(635 - 8, 713 * 0.6),
    });
  });
});

describe('mergeSkillMenu', () => {
  it('merges enabled manager skills with core commands, one entry per slug', () => {
    const managerData = {
      skills: [
        { id: '1', name: 'My Skill', enabled: true },
        { id: '2', name: 'Off One', enabled: false },
      ],
    };
    const commands = {
      commands: [
        { name: 'translate-zh-en', state: 'user-only' },
        { name: 'my-skill', description: 'core' },
      ],
    };

    const merged = mergeSkillMenu(managerData, commands);

    expect(merged.map(skill => skill.slug)).toEqual(['my-skill', 'translate-zh-en']);
    const mySkill = merged.find(skill => skill.slug === 'my-skill');
    expect(mySkill?.name).toBe('my-skill');
    expect(mySkill?.description).toBe('core');
    const translate = merged.find(skill => skill.slug === 'translate-zh-en');
    expect(translate?.state).toBe('user-only');
  });

  it('accepts the legacy examples and my_skills shape', () => {
    const merged = mergeSkillMenu(
      {
        examples: [{ id: 'a', name: 'Example Skill', enabled: true }],
        my_skills: [
          { id: 'b', name: 'Mine', enabled: true },
          { id: 'c', name: 'Disabled', enabled: false },
        ],
      },
      {},
    );

    expect(merged.map(skill => skill.slug)).toEqual(['example-skill', 'mine']);
  });
});
