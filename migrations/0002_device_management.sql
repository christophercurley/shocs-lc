-- PH3-A1: clarify durable device-management semantics.
-- "enabled" was ambiguous next to a light's physical power state. This flag
-- means whether SHOCS-LC is permitted to issue control/automation commands.

ALTER TABLE lights
    RENAME COLUMN enabled TO control_enabled;
