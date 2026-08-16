-- The per-address invite-cooldown read: a repeatedly invited address is skipped for a while,
-- counted server-wide off the append-only audit trail (kind = 'invitation_created', subject =
-- the invited address). Additive; rides the same partial-index discipline as the actor indexes.

CREATE INDEX "audit_invite_subject" ON "web"."audit_event" USING btree ("subject","created_at") WHERE kind = 'invitation_created';