/**
 * The skills this enrollment pre-offers, as the wire carries them — a plain list of names.
 * Renders nothing when the list is empty.
 */
export function SkillOfferList({ skills }: { skills: readonly string[] | undefined }) {
  if (skills === undefined || skills.length === 0) {
    return null;
  }
  return (
    <div className="flex flex-col gap-1.5">
      <h2 className="font-display text-[10px] uppercase tracking-[0.12em] text-faint">
        Offered skills
      </h2>
      <ul className="flex flex-col gap-1">
        {skills.map((name) => (
          <li
            key={name}
            className="rounded-md border border-line-soft bg-ground px-3 py-1.5 text-sm text-dim"
          >
            {name}
          </li>
        ))}
      </ul>
    </div>
  );
}
