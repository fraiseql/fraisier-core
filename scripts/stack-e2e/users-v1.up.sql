CREATE TABLE IF NOT EXISTS tb_user (
    pk_user SERIAL PRIMARY KEY,
    id UUID NOT NULL UNIQUE,
    name TEXT NOT NULL,
    email TEXT NOT NULL
);
INSERT INTO tb_user (id, name, email) VALUES
    ('11111111-1111-1111-1111-111111111111', 'Ada', 'ada@example.test'),
    ('22222222-2222-2222-2222-222222222222', 'Grace', 'grace@example.test')
ON CONFLICT (id) DO NOTHING;
CREATE OR REPLACE VIEW v_user AS
SELECT pk_user, id, jsonb_build_object('id', id, 'name', name, 'email', email) AS data
FROM tb_user;
