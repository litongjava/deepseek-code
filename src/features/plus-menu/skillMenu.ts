export interface MenuSkill {
  id: string;
  name: string;
  description?: string;
  slug: string;
  state?: string;
}

/** Desktop manager skills that are enabled, plus core slash skills; one entry per slug. */
export function mergeSkillMenu(managerData: any, commands: any): MenuSkill[] {
  const merged = new Map<string, MenuSkill>();
  const managerSkills = managerData?.skills ?? [
    ...(managerData?.examples || []),
    ...(managerData?.my_skills || []),
  ];
  for (const skill of managerSkills || []) {
    if (!skill?.enabled) continue;
    const slug = String(skill.name || '').toLowerCase().replace(/\s+/g, '-');
    if (!slug) continue;
    merged.set(slug, {
      id: skill.id ?? slug,
      name: skill.name,
      description: skill.description,
      slug,
    });
  }
  // A core command wins a slug collision: that is what the turn resolves first.
  for (const command of commands?.commands || []) {
    const slug = command?.name;
    if (!slug) continue;
    merged.set(slug, {
      id: command.id ?? slug,
      name: command.name,
      description: command.description,
      slug,
      state: command.state,
    });
  }
  return [...merged.values()].sort((a, b) =>
    a.name < b.name ? -1 : a.name > b.name ? 1 : 0);
}
