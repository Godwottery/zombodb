select table_name, query, total from zdb_multi_search(ARRAY['so_posts', 'so_users', 'words'], 'cheese');