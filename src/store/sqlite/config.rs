use rusqlite::OptionalExtension;

use crate::store::models::{
    ContextRecord, GenericConfigRecord, GeofenceRecord, PresetRecord, ShoppingRecord, StoreRecord,
    ViewRecord,
};

use super::{map_err, BoxErr, SqliteConfigStore};

fn view_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ViewRecord> {
    Ok(ViewRecord {
        id: row.get(0)?,
        label: row.get(1)?,
        icon: row.get(2)?,
        filter: row.get(3)?,
        group_by: row.get(4)?,
        context_filtered: row.get(5)?,
        display_mode: row.get(6)?,
        sort_order: row.get(7)?,
        origin: row.get(8)?,
        user_modified: row.get(9)?,
        hidden: row.get(10)?,
        template_version: row.get(11)?,
    })
}

fn context_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ContextRecord> {
    let prefixes_json: String = row.get(2)?;
    let project_prefixes: Vec<String> = serde_json::from_str(&prefixes_json).unwrap_or_default();
    Ok(ContextRecord {
        id: row.get(0)?,
        label: row.get(1)?,
        project_prefixes,
        color: row.get(3)?,
        icon: row.get(4)?,
        sort_order: row.get(5)?,
    })
}

fn preset_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PresetRecord> {
    Ok(PresetRecord {
        id: row.get(0)?,
        label: row.get(1)?,
        raw_suffix: row.get(2)?,
        sort_order: row.get(3)?,
    })
}

fn store_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoreRecord> {
    Ok(StoreRecord {
        id: row.get(0)?,
        label: row.get(1)?,
        tag: row.get(2)?,
        sort_order: row.get(3)?,
    })
}

fn geofence_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<GeofenceRecord> {
    Ok(GeofenceRecord {
        id: row.get(0)?,
        label: row.get(1)?,
        latitude: row.get(2)?,
        longitude: row.get(3)?,
        radius: row.get(4)?,
        geofence_type: row.get(5)?,
        context_id: row.get(6)?,
        view_id: row.get(7)?,
        store_tag: row.get(8)?,
    })
}

fn shopping_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ShoppingRecord> {
    let tags_json: String = row.get(1)?;
    let default_tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();
    Ok(ShoppingRecord {
        project: row.get(0)?,
        default_tags,
    })
}

fn generic_config_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<GenericConfigRecord> {
    Ok(GenericConfigRecord {
        version: row.get(0)?,
        items_json: row.get(1)?,
    })
}

impl SqliteConfigStore {
    pub(super) async fn list_views_impl(&self, user_id: &str) -> anyhow::Result<Vec<ViewRecord>> {
        let user_id = user_id.to_string();
        let result = self
            .conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, label, icon, filter, group_by, context_filtered,
                            display_mode, sort_order,
                            origin, user_modified, hidden, template_version
                     FROM views WHERE user_id = ?1 AND hidden = 0 ORDER BY sort_order",
                )?;
                let rows = stmt
                    .query_map([&user_id], view_from_row)?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<_, BoxErr>(rows)
            })
            .await
            .map_err(map_err)?;
        Ok(result)
    }

    pub(super) async fn list_views_all_impl(
        &self,
        user_id: &str,
    ) -> anyhow::Result<Vec<ViewRecord>> {
        let user_id = user_id.to_string();
        let result = self
            .conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, label, icon, filter, group_by, context_filtered,
                            display_mode, sort_order,
                            origin, user_modified, hidden, template_version
                     FROM views WHERE user_id = ?1 ORDER BY sort_order",
                )?;
                let rows = stmt
                    .query_map([&user_id], view_from_row)?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<_, BoxErr>(rows)
            })
            .await
            .map_err(map_err)?;
        Ok(result)
    }

    pub(super) async fn upsert_view_impl(
        &self,
        user_id: &str,
        view: &ViewRecord,
    ) -> anyhow::Result<()> {
        let user_id = user_id.to_string();
        let view = view.clone();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO views (id, user_id, label, icon, filter, group_by,
                                        context_filtered, display_mode, sort_order,
                                        origin, user_modified, hidden, template_version)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                     ON CONFLICT (user_id, id) DO UPDATE SET
                       label = ?3, icon = ?4, filter = ?5, group_by = ?6,
                       context_filtered = ?7, display_mode = ?8, sort_order = ?9,
                       origin = ?10, user_modified = ?11, hidden = ?12, template_version = ?13",
                    rusqlite::params![
                        view.id,
                        user_id,
                        view.label,
                        view.icon,
                        view.filter,
                        view.group_by,
                        view.context_filtered,
                        view.display_mode,
                        view.sort_order,
                        view.origin,
                        view.user_modified,
                        view.hidden,
                        view.template_version
                    ],
                )?;
                Ok::<_, BoxErr>(())
            })
            .await
            .map_err(map_err)?;
        Ok(())
    }

    pub(super) async fn delete_view_impl(&self, user_id: &str, id: &str) -> anyhow::Result<bool> {
        let user_id = user_id.to_string();
        let id = id.to_string();
        let result = self
            .conn
            .call(move |conn| {
                let origin: Option<String> = conn
                    .query_row(
                        "SELECT origin FROM views WHERE user_id = ?1 AND id = ?2",
                        rusqlite::params![&user_id, &id],
                        |row| row.get(0),
                    )
                    .optional()?;

                match origin.as_deref() {
                    Some("builtin") => {
                        let count = conn.execute(
                            "UPDATE views SET hidden = 1 WHERE user_id = ?1 AND id = ?2",
                            rusqlite::params![user_id, id],
                        )?;
                        Ok::<_, BoxErr>(count > 0)
                    }
                    Some(_) | None => {
                        let count = conn.execute(
                            "DELETE FROM views WHERE user_id = ?1 AND id = ?2",
                            rusqlite::params![user_id, id],
                        )?;
                        Ok::<_, BoxErr>(count > 0)
                    }
                }
            })
            .await
            .map_err(map_err)?;
        Ok(result)
    }

    pub(super) async fn list_contexts_impl(
        &self,
        user_id: &str,
    ) -> anyhow::Result<Vec<ContextRecord>> {
        let user_id = user_id.to_string();
        let result = self
            .conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, label, project_prefixes, color, icon, sort_order
                     FROM contexts WHERE user_id = ?1 ORDER BY sort_order",
                )?;
                let rows = stmt
                    .query_map([&user_id], context_from_row)?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<_, BoxErr>(rows)
            })
            .await
            .map_err(map_err)?;
        Ok(result)
    }

    pub(super) async fn upsert_context_impl(
        &self,
        user_id: &str,
        ctx: &ContextRecord,
    ) -> anyhow::Result<()> {
        let user_id = user_id.to_string();
        let ctx = ctx.clone();
        self.conn
            .call(move |conn| {
                let prefixes_json =
                    serde_json::to_string(&ctx.project_prefixes).map_err(|e| Box::new(e) as BoxErr)?;
                conn.execute(
                    "INSERT INTO contexts (id, user_id, label, project_prefixes, color, icon, sort_order)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT (user_id, id) DO UPDATE SET
                       label = ?3, project_prefixes = ?4, color = ?5, icon = ?6, sort_order = ?7",
                    rusqlite::params![
                        ctx.id, user_id, ctx.label, prefixes_json,
                        ctx.color, ctx.icon, ctx.sort_order
                    ],
                )?;
                Ok::<_, BoxErr>(())
            })
            .await
            .map_err(map_err)?;
        Ok(())
    }

    pub(super) async fn delete_context_impl(
        &self,
        user_id: &str,
        id: &str,
    ) -> anyhow::Result<bool> {
        let user_id = user_id.to_string();
        let id = id.to_string();
        let result = self
            .conn
            .call(move |conn| {
                let count = conn.execute(
                    "DELETE FROM contexts WHERE user_id = ?1 AND id = ?2",
                    rusqlite::params![user_id, id],
                )?;
                Ok::<_, BoxErr>(count > 0)
            })
            .await
            .map_err(map_err)?;
        Ok(result)
    }

    pub(super) async fn list_presets_impl(
        &self,
        user_id: &str,
    ) -> anyhow::Result<Vec<PresetRecord>> {
        let user_id = user_id.to_string();
        let result = self
            .conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, label, raw_suffix, sort_order
                     FROM presets WHERE user_id = ?1 ORDER BY sort_order",
                )?;
                let rows = stmt
                    .query_map([&user_id], preset_from_row)?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<_, BoxErr>(rows)
            })
            .await
            .map_err(map_err)?;
        Ok(result)
    }

    pub(super) async fn upsert_preset_impl(
        &self,
        user_id: &str,
        preset: &PresetRecord,
    ) -> anyhow::Result<()> {
        let user_id = user_id.to_string();
        let preset = preset.clone();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO presets (id, user_id, label, raw_suffix, sort_order)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT (user_id, id) DO UPDATE SET
                       label = ?3, raw_suffix = ?4, sort_order = ?5",
                    rusqlite::params![
                        preset.id,
                        user_id,
                        preset.label,
                        preset.raw_suffix,
                        preset.sort_order
                    ],
                )?;
                Ok::<_, BoxErr>(())
            })
            .await
            .map_err(map_err)?;
        Ok(())
    }

    pub(super) async fn delete_preset_impl(&self, user_id: &str, id: &str) -> anyhow::Result<bool> {
        let user_id = user_id.to_string();
        let id = id.to_string();
        let result = self
            .conn
            .call(move |conn| {
                let count = conn.execute(
                    "DELETE FROM presets WHERE user_id = ?1 AND id = ?2",
                    rusqlite::params![user_id, id],
                )?;
                Ok::<_, BoxErr>(count > 0)
            })
            .await
            .map_err(map_err)?;
        Ok(result)
    }

    pub(super) async fn list_stores_impl(&self, user_id: &str) -> anyhow::Result<Vec<StoreRecord>> {
        let user_id = user_id.to_string();
        let result = self
            .conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, label, tag, sort_order
                     FROM stores WHERE user_id = ?1 ORDER BY sort_order",
                )?;
                let rows = stmt
                    .query_map([&user_id], store_from_row)?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<_, BoxErr>(rows)
            })
            .await
            .map_err(map_err)?;
        Ok(result)
    }

    pub(super) async fn upsert_store_impl(
        &self,
        user_id: &str,
        store: &StoreRecord,
    ) -> anyhow::Result<()> {
        let user_id = user_id.to_string();
        let store = store.clone();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO stores (id, user_id, label, tag, sort_order)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT (user_id, id) DO UPDATE SET
                       label = ?3, tag = ?4, sort_order = ?5",
                    rusqlite::params![store.id, user_id, store.label, store.tag, store.sort_order],
                )?;
                Ok::<_, BoxErr>(())
            })
            .await
            .map_err(map_err)?;
        Ok(())
    }

    pub(super) async fn delete_store_impl(&self, user_id: &str, id: &str) -> anyhow::Result<bool> {
        let user_id = user_id.to_string();
        let id = id.to_string();
        let result = self
            .conn
            .call(move |conn| {
                let count = conn.execute(
                    "DELETE FROM stores WHERE user_id = ?1 AND id = ?2",
                    rusqlite::params![user_id, id],
                )?;
                Ok::<_, BoxErr>(count > 0)
            })
            .await
            .map_err(map_err)?;
        Ok(result)
    }

    pub(super) async fn get_shopping_config_impl(
        &self,
        user_id: &str,
    ) -> anyhow::Result<Option<ShoppingRecord>> {
        let user_id = user_id.to_string();
        let result = self
            .conn
            .call(move |conn| {
                let result = conn.query_row(
                    "SELECT project, default_tags FROM shopping_config WHERE user_id = ?1",
                    [&user_id],
                    shopping_from_row,
                );
                match result {
                    Ok(record) => Ok::<_, BoxErr>(Some(record)),
                    Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                    Err(e) => Err(e.into()),
                }
            })
            .await
            .map_err(map_err)?;
        Ok(result)
    }

    pub(super) async fn upsert_shopping_config_impl(
        &self,
        user_id: &str,
        config: &ShoppingRecord,
    ) -> anyhow::Result<()> {
        let user_id = user_id.to_string();
        let config = config.clone();
        self.conn
            .call(move |conn| {
                let tags_json = serde_json::to_string(&config.default_tags)
                    .map_err(|e| Box::new(e) as BoxErr)?;
                conn.execute(
                    "INSERT INTO shopping_config (user_id, project, default_tags)
                     VALUES (?1, ?2, ?3)
                     ON CONFLICT (user_id) DO UPDATE SET project = ?2, default_tags = ?3",
                    rusqlite::params![user_id, config.project, tags_json],
                )?;
                Ok::<_, BoxErr>(())
            })
            .await
            .map_err(map_err)?;
        Ok(())
    }

    pub(super) async fn delete_shopping_config_impl(&self, user_id: &str) -> anyhow::Result<bool> {
        let user_id = user_id.to_string();
        let result = self
            .conn
            .call(move |conn| {
                let count = conn.execute(
                    "DELETE FROM shopping_config WHERE user_id = ?1",
                    rusqlite::params![user_id],
                )?;
                Ok::<_, BoxErr>(count > 0)
            })
            .await
            .map_err(map_err)?;
        Ok(result)
    }

    pub(super) async fn list_geofences_impl(
        &self,
        user_id: &str,
    ) -> anyhow::Result<Vec<GeofenceRecord>> {
        let user_id = user_id.to_string();
        let result = self
            .conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, label, latitude, longitude, radius, type, context_id, view_id, store_tag
                     FROM geofences WHERE user_id = ?1 ORDER BY id",
                )?;
                let rows = stmt.query_map(rusqlite::params![user_id], geofence_from_row)?;
                let mut out = Vec::new();
                for row in rows {
                    out.push(row?);
                }
                Ok::<_, BoxErr>(out)
            })
            .await
            .map_err(map_err)?;
        Ok(result)
    }

    pub(super) async fn upsert_geofence_impl(
        &self,
        user_id: &str,
        geofence: &GeofenceRecord,
    ) -> anyhow::Result<()> {
        let user_id = user_id.to_string();
        let geofence = geofence.clone();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO geofences (user_id, id, label, latitude, longitude, radius, type, context_id, view_id, store_tag)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                     ON CONFLICT (user_id, id) DO UPDATE SET
                       label = excluded.label,
                       latitude = excluded.latitude,
                       longitude = excluded.longitude,
                       radius = excluded.radius,
                       type = excluded.type,
                       context_id = excluded.context_id,
                       view_id = excluded.view_id,
                       store_tag = excluded.store_tag",
                    rusqlite::params![
                        user_id,
                        geofence.id,
                        geofence.label,
                        geofence.latitude,
                        geofence.longitude,
                        geofence.radius,
                        geofence.geofence_type,
                        geofence.context_id,
                        geofence.view_id,
                        geofence.store_tag,
                    ],
                )?;
                Ok::<_, BoxErr>(())
            })
            .await
            .map_err(map_err)?;
        Ok(())
    }

    pub(super) async fn delete_geofence_impl(
        &self,
        user_id: &str,
        id: &str,
    ) -> anyhow::Result<bool> {
        let user_id = user_id.to_string();
        let id = id.to_string();
        let result = self
            .conn
            .call(move |conn| {
                let count = conn.execute(
                    "DELETE FROM geofences WHERE user_id = ?1 AND id = ?2",
                    rusqlite::params![user_id, id],
                )?;
                Ok::<_, BoxErr>(count > 0)
            })
            .await
            .map_err(map_err)?;
        Ok(result)
    }

    pub(super) async fn get_config_impl(
        &self,
        user_id: &str,
        config_type: &str,
    ) -> anyhow::Result<Option<GenericConfigRecord>> {
        let user_id = user_id.to_string();
        let config_type = config_type.to_string();
        let result = self
            .conn
            .call(move |conn| {
                let result = conn.query_row(
                    "SELECT version, items FROM config WHERE user_id = ?1 AND config_type = ?2",
                    rusqlite::params![user_id, config_type],
                    generic_config_from_row,
                );
                match result {
                    Ok(record) => Ok::<_, BoxErr>(Some(record)),
                    Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                    Err(e) => Err(e.into()),
                }
            })
            .await
            .map_err(map_err)?;
        Ok(result)
    }

    pub(super) async fn upsert_config_impl(
        &self,
        user_id: &str,
        config_type: &str,
        record: &GenericConfigRecord,
    ) -> anyhow::Result<()> {
        let user_id = user_id.to_string();
        let config_type = config_type.to_string();
        let record = record.clone();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO config (config_type, user_id, version, items)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT (user_id, config_type) DO UPDATE SET version = ?3, items = ?4",
                    rusqlite::params![config_type, user_id, record.version, record.items_json],
                )?;
                Ok::<_, BoxErr>(())
            })
            .await
            .map_err(map_err)?;
        Ok(())
    }

    pub(super) async fn delete_config_item_impl(
        &self,
        user_id: &str,
        config_type: &str,
        item_id: &str,
    ) -> anyhow::Result<bool> {
        let user_id = user_id.to_string();
        let config_type = config_type.to_string();
        let item_id = item_id.to_string();
        let result = self
            .conn
            .call(move |conn| {
                let result = conn.query_row(
                    "SELECT items FROM config WHERE user_id = ?1 AND config_type = ?2",
                    rusqlite::params![user_id, config_type],
                    |row| row.get::<_, String>(0),
                );

                let items_json = match result {
                    Ok(json) => json,
                    Err(rusqlite::Error::QueryReturnedNoRows) => return Ok::<_, BoxErr>(false),
                    Err(e) => return Err(e.into()),
                };

                let mut items: Vec<serde_json::Value> =
                    serde_json::from_str(&items_json).unwrap_or_default();
                let original_len = items.len();
                items.retain(|item| item.get("id").and_then(|v| v.as_str()) != Some(&item_id));

                if items.len() == original_len {
                    return Ok(false);
                }

                let new_json = serde_json::to_string(&items).map_err(|e| Box::new(e) as BoxErr)?;
                conn.execute(
                    "UPDATE config SET items = ?1 WHERE user_id = ?2 AND config_type = ?3",
                    rusqlite::params![new_json, user_id, config_type],
                )?;
                Ok(true)
            })
            .await
            .map_err(map_err)?;
        Ok(result)
    }
}
