CREATE TABLE IF NOT EXISTS tb_order (
    pk_order SERIAL PRIMARY KEY,
    id UUID NOT NULL UNIQUE,
    total NUMERIC(10, 2) NOT NULL,
    customer_name TEXT NOT NULL
);
INSERT INTO tb_order (id, total, customer_name) VALUES
    ('aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa', 42.50, 'Ada'),
    ('bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb', 7.25, 'Grace')
ON CONFLICT (id) DO NOTHING;
CREATE OR REPLACE VIEW v_order AS
SELECT pk_order, id,
       jsonb_build_object('id', id, 'total', total, 'customer_name', customer_name) AS data
FROM tb_order;
