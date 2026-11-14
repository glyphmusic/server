use std::{collections::HashMap, rc::Rc};
use futures::StreamExt;
use glyph_server_core::{chunks::{Manifest,Window,CHUNK_BYTES},media::{AudioVariant,byte_range}};
use worker::{Bucket,D1Database,Headers,Response,Result,wasm_bindgen::JsValue};

pub const RESERVE_GROUP: &str = r#"WITH
parts AS MATERIALIZED (SELECT json_extract(value,'$.key') AS key,json_extract(value,'$.size') AS size FROM json_each(?1)),
missing AS MATERIALIZED (SELECT * FROM parts WHERE NOT EXISTS(SELECT 1 FROM audio_cache WHERE key=parts.key)),
budget AS MATERIALIZED (SELECT COALESCE((SELECT json_extract(value,'$.audio_cache_max_gb')*1000000000 FROM server_settings WHERE key='limits'),?4) AS cap),
capacity AS MATERIALIZED (SELECT COALESCE((SELECT SUM(size) FROM audio_cache),0)+COALESCE((SELECT SUM(size) FROM missing),0) AS needed)
INSERT INTO audio_cache(key,size,created,state,generation)
SELECT missing.key,missing.size,?2,'pending',?3 FROM missing,budget,capacity
WHERE (SELECT ready FROM audio_cache_inventory WHERE id=1)=1 AND needed<=cap-MIN(500000000,cap/2)
ON CONFLICT(key) DO NOTHING"#;
fn s(v:&str)->JsValue {JsValue::from_str(v)}
fn n(v:u64)->JsValue {JsValue::from_f64(v as f64)}

async fn piece(bucket:&Bucket,key:&str,w:&Window)->Result<Vec<u8>> {
    let obj=bucket.get(key).range(worker::Range::OffsetWithLength{offset:w.offset,length:w.length}).execute().await?
        .ok_or_else(||worker::Error::RustError("audio chunk unavailable".into()))?;
    let bytes=obj.body().ok_or_else(||worker::Error::RustError("audio chunk empty".into()))?.bytes().await?;
    if bytes.len() as u64!=w.length {return Err(worker::Error::RustError("audio chunk length mismatch".into()));}
    Ok(bytes)
}

pub async fn cached(ctx:&worker::Context,db:&D1Database,bucket:Bucket,id:&str,quality:AudioVariant,range:Option<&str>,head:bool,days:u32)->Result<Option<Response>> {
    #[derive(serde::Deserialize)]struct Row{manifest:String,created:u64}
    let row=db.prepare("SELECT manifest,created FROM audio_variants WHERE track_id=?1 AND quality=?2")
        .bind(&[s(id),s(quality.name())])?.first::<Row>(None).await?;
    let Some(row)=row else{return Ok(None)};
    if worker::Date::now().as_millis().saturating_sub(row.created)>u64::from(days)*86_400_000 {return Ok(None);}
    let Ok(manifest)=serde_json::from_str::<Manifest>(&row.manifest) else{return Ok(None)};
    if !manifest.valid(id,quality){return Ok(None);}
    let keys:Vec<_>=manifest.parts.iter().map(|p|p.key.clone()).collect();
    let keys_json=serde_json::to_string(&keys)?;
    #[derive(serde::Deserialize)]struct Count{count:usize}
    let count=db.prepare("SELECT COUNT(*) AS count FROM audio_cache WHERE state='ready' AND key IN(SELECT value FROM json_each(?1))")
        .bind(&[s(&keys_json)])?.first::<Count>(None).await?;
    if count.map(|v|v.count)!=Some(keys.len()){return Ok(None);}
    let (start,end,partial)=if let Some(range)=range {
        let Some((a,b))=byte_range(range,manifest.total) else{
            let mut r=Response::error("range not satisfiable",416)?;
            r.headers_mut().set("content-range",&format!("bytes */{}",manifest.total))?;
            return Ok(Some(r));
        };
        (a,b,true)
    }else{(0,manifest.total-1,false)};
    let headers=Headers::new();
    headers.set("content-type","audio/mp4")?;
    headers.set("content-length",&(end-start+1).to_string())?;
    headers.set("accept-ranges","bytes")?;
    headers.set("cache-control","no-store")?;
    headers.set("x-content-type-options","nosniff")?;
    headers.set("x-glyph-audio-quality",quality.name())?;
    headers.set("x-glyph-cache","R2-CHUNKS")?;
    if partial{headers.set("content-range",&format!("bytes {start}-{end}/{}",manifest.total))?;}
    let status=if partial{206}else{200};
    if head{return Ok(Some(Response::empty()?.with_status(status).with_headers(headers)));}
    let mut windows=manifest.windows(start,end).unwrap().into_iter();
    let first=windows.next().unwrap();
    let first=match piece(&bucket,&manifest.parts[first.part].key,&first).await{Ok(bytes)=>bytes,Err(_)=>return Ok(None)};
    // Count popularity per song, so its later chunks do not look artificially cold.
    if start == 0 {
        let touch=db.prepare("UPDATE audio_cache SET hits=MIN(hits+1,2147483647),last_access=?1 WHERE state='ready' AND key IN(SELECT value FROM json_each(?2))")
            .bind(&[n(worker::Date::now().as_millis()),s(&keys_json)])?;
        ctx.wait_until(async move { let _=touch.run().await; });
    }
    let bucket=Rc::new(bucket);
    let remaining=futures::stream::iter(windows).then(move|w|{
        let bucket=bucket.clone();let key=manifest.parts[w.part].key.clone();
        async move{piece(&bucket,&key,&w).await}
    });
    let stream=futures::stream::once(async{Ok(first)}).chain(remaining);
    Ok(Some(Response::from_stream(stream)?.with_status(status).with_headers(headers)))
}

pub async fn store(db:&D1Database,bucket:&Bucket,id:&str,quality:AudioVariant,bytes:Vec<u8>,days:u32,cap:u64,cur_usage:Option<u64>)->Result<()> {
    if !super::audio_cache::inventory(db,bucket).await?{return Ok(());}
    let Some(manifest)=Manifest::build(id,quality,&bytes) else{return Ok(())};
    let target = super::audio_cache::target(cap).saturating_sub(manifest.total);
    if cur_usage.is_none_or(|u| u > target) {
        super::audio_cache::trim(db,bucket,days,target).await?;
    }
    let mut entropy=[0u8;16];getrandom::getrandom(&mut entropy).map_err(|_|worker::Error::RustError("audio entropy unavailable".into()))?;
    let token:String=entropy.iter().map(|b|format!("{b:02x}")).collect();
    let parts=serde_json::to_string(&manifest.parts)?;
    let now=worker::Date::now().as_millis();
    db.prepare(RESERVE_GROUP).bind(&[s(&parts),n(now),s(&token),n(cap)])?.run().await?;
    #[derive(serde::Deserialize)]struct State{key:String,state:String,generation:String}
    let rows=db.prepare("SELECT key,state,generation FROM audio_cache WHERE key IN(SELECT json_extract(value,'$.key') FROM json_each(?1))")
        .bind(&[s(&parts)])?.all().await?.results::<State>()?;
    let states:HashMap<_,_>=rows.into_iter().map(|r|(r.key.clone(),r)).collect();
    for (part,chunk) in manifest.parts.iter().zip(bytes.chunks(CHUNK_BYTES as usize)) {
        let Some(row)=states.get(&part.key) else{return Ok(())};
        if row.state=="ready"{continue;}
        if row.state!="pending" || row.generation!=token{return Ok(());}
        let metadata=HashMap::from([("generation".to_string(),token.clone())]);
        bucket.put(&part.key,chunk.to_vec()).custom_metadata(metadata).execute().await?;
    }
    let s1 = db.prepare("UPDATE audio_cache SET state='ready',seen=(SELECT scan_id FROM audio_cache_inventory WHERE id=1) WHERE generation=?1 AND state='pending'")
        .bind(&[s(&token)])?;
    // Publish only after every part is complete; partial uploads are never
    // advertised as a playable representation.
    let s2 = db.prepare("INSERT INTO audio_variants(track_id,quality,manifest,created) VALUES(?1,?2,?3,?4) ON CONFLICT(track_id,quality) DO UPDATE SET manifest=excluded.manifest,created=excluded.created")
        .bind(&[s(id),s(quality.name()),s(&serde_json::to_string(&manifest)?),n(now)])?;
    let s3 = db.prepare("DELETE FROM audio_variant_jobs WHERE track_id=?1").bind(&[s(id)])?;
    let _ = db.batch(vec![s1, s2, s3]).await?;
    Ok(())
}

