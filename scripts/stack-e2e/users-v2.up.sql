CREATE TABLE IF NOT EXISTS tb_user (
    pk_user SERIAL PRIMARY KEY,
    id UUID NOT NULL UNIQUE,
    name TEXT NOT NULL,
    email TEXT NOT NULL
);
ALTER TABLE tb_user ADD COLUMN IF NOT EXISTS nickname TEXT NOT NULL DEFAULT 'n/a';
CREATE OR REPLACE VIEW v_user AS
SELECT pk_user, id,
       jsonb_build_object('id', id, 'name', name, 'email', email, 'nickname', nickname) AS data
FROM tb_user;
