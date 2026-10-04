'use strict';

const $ = (s, root = document) => root.querySelector(s);
const $$ = (s, root = document) => [...root.querySelectorAll(s)];
const esc = value => String(value ?? '').replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
const clone = value => JSON.parse(JSON.stringify(value));
const state = {data: null, selected: null, editorProject: null, spec: null, pendingJson: null, dirty: false, draftDirty: false, preferredVoiceLoaded: false, pipelineDirty: false, pipelineLoaded: false, view: 'studio', graph: null, snapshot: null, graphEvent: null, graphRequest: 0, polling: null, refreshing: false};
const labels = {draft:'Сценарий',ready:'Готов',rendered:'Собран',rendering:'Рендер',queued:'В очереди',pending:'Ожидание',running:'В работе',scheduled:'Запланировано',succeeded:'Завершено',failed:'Ошибка',cancelled:'Отменено',published:'Опубликован',render:'Рендер',publish:'Публикация',export:'Экспорт',dry_run:'Проверено',unknown:'Результат неизвестен',HYPOTHESIS:'Гипотеза',OBSERVATION:'Наблюдение'};
const profiles = {shorts:'YouTube Shorts',reels:'Instagram Reels',tiktok:'TikTok',vk:'VK Клипы',landscape:'YouTube · 16:9',square:'Квадрат · 1:1'};
const activeStates = new Set(['queued','pending','running','scheduled','rendering']);
const badge = name => `<span class="status ${esc(name)}">${esc(labels[name] || name)}</span>`;
const empty = (icon, title, text, action = '') => `<div class="empty"><span aria-hidden="true">${icon}</span><h3>${esc(title)}</h3><p>${esc(text)}</p>${action}</div>`;
const niceDate = (value, detailed = false) => {if (!value) return '—'; const d = new Date(value); return Number.isNaN(d.getTime()) ? String(value) : d.toLocaleString('ru-RU', detailed ? {day:'2-digit',month:'short',hour:'2-digit',minute:'2-digit',second:'2-digit'} : {day:'2-digit',month:'short',hour:'2-digit',minute:'2-digit'});};
const short = (s, max = 25) => {s=String(s || '');return s.length > max ? s.slice(0,max-1)+'…' : s;};
const projectTitle = id => state.data?.projects.find(p => p.id === id)?.spec.title || 'Проект ' + short(id,12);
const safeUrl = value => {try {const u=new URL(String(value));return ['https:','http:'].includes(u.protocol) ? u.href : '#';} catch {return '#';}};
const localPath = (path, prefix) => {if(typeof path !== 'string' || path.includes('..') || path.includes('\\') || path.startsWith('/')) return null;const clean=path.startsWith(prefix+'/')?path.slice(prefix.length+1):path;if(!clean || clean.includes(':')) return null;return '/'+(prefix === 'renders' ? 'media' : prefix)+'/'+clean.split('/').map(encodeURIComponent).join('/');};

function toast(message, error = false) {const el=document.createElement('div');el.className='toast'+(error?' error':'');el.textContent=message;$('#toast-region').append(el);setTimeout(()=>el.remove(),error?11000:6000);}
async function api(path, options = {}) {
  const config={...options,headers:{...(options.headers || {})}};
  if(config.body && !(config.body instanceof FormData)) {config.headers['Content-Type']='application/json';config.body=JSON.stringify(config.body);}
  const response=await fetch(path,config);const text=await response.text();let data;
  try {data=text?JSON.parse(text):{};} catch {data={error:text || 'Пустой ответ сервера'};}
  if(!response.ok) {const detail=typeof data.error==='string'?data.error:typeof data.message==='string'?data.message:text;throw new Error(`${response.status}: ${detail || response.statusText}`);}
  return data;
}
async function withBusy(button, action) {if(button?.disabled) return; if(button) button.disabled=true;try {await action();} catch(e) {toast(e.message || String(e),true);} finally {if(button?.isConnected) button.disabled=false;}}
async function refreshState({quiet=false}={}) {
  if(state.refreshing) return;state.refreshing=true;
  try {
    state.data=await api('/api/state');applyPreferredVoice();$('#global-error').hidden=true;$('#connection').textContent='Локально';$('#connection').className='connection online';
    $('#project-count').textContent=state.data.projects.length;$('#job-count').textContent=state.data.jobs.filter(j=>activeStates.has(j.state)).length;
    $('#version').textContent=`RustClip ${state.data.capabilities?.version || ''} · open source`;
    if(state.selected && !state.data.projects.some(p=>p.id===state.selected)){state.selected=null;state.spec=null;state.editorProject=null;state.dirty=false;}
    renderProjects();renderCapabilities();renderTrends();renderMoney();renderJobs();renderPipeline();syncProjectSelects();
    if(!state.selected && state.data.projects.length) selectProject(state.data.projects[0].id,false);
    else if(!state.selected) renderEditor();
    else {
      const current=state.data.projects.find(p=>p.id===state.selected);
      if(!state.dirty && current && current.revision!==state.editorProject?.revision){state.editorProject=clone(current);state.spec=clone(current.spec);renderEditor();}
      else updatePreview();
    }
    const ollamaOption=$('#draft-generator option[value="true"]');ollamaOption.disabled=!state.data.capabilities?.ollama_configured;
    if(ollamaOption.disabled) $('#draft-generator').value='false';
    $('#pipeline-generator option[value="true"]').disabled=!state.data.capabilities?.ollama_configured;
  } catch(e) {
    $('#connection').textContent='Нет связи';$('#connection').className='connection offline';
    if(!quiet){$('#global-error').textContent='Не удалось загрузить студию. '+e.message;$('#global-error').hidden=false;}
  } finally {
    state.refreshing=false;clearTimeout(state.polling);
    if(state.data?.jobs.some(j=>activeStates.has(j.state))||state.data?.pipeline?.status==='running') state.polling=setTimeout(()=>refreshState({quiet:true}),4000);
    else if(state.data?.pipeline?.enabled) state.polling=setTimeout(()=>refreshState({quiet:true}),15000);
  }
}
function applyPreferredVoice() {
  if(state.preferredVoiceLoaded)return;
  const voice=state.data?.capabilities?.renderer?.preferred_voice;
  if(!['piper','espeak','none'].includes(voice))return;
  state.preferredVoiceLoaded=true;
  if(!state.draftDirty){const select=$('#draft-form').elements.voice;for(const option of select.options)option.defaultSelected=option.value===voice;select.value=voice;}
  if(!state.pipelineLoaded&&!state.pipelineDirty&&!state.data?.pipeline?.config)pipelineDefaults.voice=voice;
}
function showView(view) {
  if(!['studio','trends','pipeline','graph','money','jobs'].includes(view)) view='studio';state.view=view;
  $$('.view').forEach(el=>el.hidden=el.id!=='view-'+view);$$('.nav-item').forEach(el=>el.classList.toggle('active',el.dataset.view===view));
  $('#view-title').textContent={studio:'Студия',trends:'Сигналы',pipeline:'Автоконвейер',graph:'Граф переходов',money:'Экономика',jobs:'Очередь'}[view];
  if(view==='graph') {if(state.selected) $('#graph-project').value=state.selected;loadGraph();}
}
function setDirty() {state.dirty=true;const el=$('#dirty-label');if(el){el.textContent='Есть несохранённые изменения';el.classList.add('dirty');}const json=$('#spec-json');if(json && state.pendingJson===null && document.activeElement!==json) json.value=JSON.stringify(state.spec,null,2);updatePreview();}
function selectProject(id, check=true) {
  if(id===state.selected) return;
  if(check && state.dirty && !confirm('В проекте есть несохранённые изменения. Перейти к другому проекту?')) return;
  const project=state.data?.projects.find(p=>p.id===id);if(!project) return;
  state.selected=id;state.editorProject=clone(project);state.spec=clone(project.spec);state.pendingJson=null;state.dirty=false;renderProjects();renderEditor();
}
function renderProjects() {
  if(!state.data) return;
  $('#project-list').innerHTML=state.data.projects.length?state.data.projects.map(p=>`<button class="project-card ${p.id===state.selected?'selected':''}" data-project="${esc(p.id)}"><div class="project-thumb"></div><h3>${esc(p.spec.title)}</h3><div class="project-meta"><span>${esc(profiles[p.spec.profile] || p.spec.profile)}</span>${badge(p.status)}</div><div class="project-meta" style="margin-top:9px"><span>${p.spec.scenes.length} сцен · ${Math.round(p.spec.scenes.reduce((n,s)=>n+s.duration_s,0))} сек</span><span>v${p.revision}</span></div></button>`).join(''):empty('▧','Пока чистый лист','Создайте сценарий или откройте демо.');
}
function capabilityEntries(value, prefix='') {if(value==null) return [];if(typeof value==='boolean') return [prefix.replace(/\.configured$/,'')+': '+(value?'готово':'не настроено')];if(typeof value==='string'||typeof value==='number') return [prefix?prefix+': '+value:String(value)];if(Array.isArray(value)) return [];return Object.entries(value).filter(([k])=>!/(path|token|secret|key)/i.test(k)).flatMap(([k,v])=>capabilityEntries(v,prefix?prefix+'.'+k:k));}
function renderCapabilities() {const c=state.data?.capabilities || {};const list=[...capabilityEntries(c.renderer),...capabilityEntries(c.publish),`Ollama: ${c.ollama_configured?'настроена':'шаблон без модели'}`];$('#studio-capabilities').innerHTML='<strong>Инструменты на этом компьютере</strong>'+list.map(v=>`<div class="capability-line">${esc(v)}</div>`).join('');}
function profileOptions(current) {return Object.entries(profiles).map(([v,label])=>`<option value="${v}" ${v===current?'selected':''}>${esc(label)}</option>`).join('');}
function sceneMarkup(scene,i) {return `<article class="scene-card" data-scene="${i}"><div class="scene-top"><span class="scene-number">${String(i+1).padStart(2,'0')}</span><strong>Сцена ${i+1}</strong><button class="scene-remove" data-remove-scene="${i}" aria-label="Удалить сцену ${i+1}" type="button">×</button></div><div class="form-grid"><label>Заголовок<input data-scene-field="title" value="${esc(scene.title)}" maxlength="100" required></label><label>Секунды<input data-scene-field="duration_s" value="${esc(scene.duration_s)}" type="number" min="1" max="30" step="0.1" required></label></div><label>Текст на экране<textarea data-scene-field="text" rows="2" maxlength="400">${esc(scene.text)}</textarea></label><label>Озвучка<textarea data-scene-field="narration" rows="2" maxlength="700">${esc(scene.narration)}</textarea></label><div class="asset-row"><button class="button secondary" data-upload-scene="${i}" type="button">+ Видео / изображение</button>${scene.asset?`<code>${esc(scene.asset)}</code><button class="text-button" data-clear-scene="${i}" type="button">Убрать</button>`:'<span class="helper">Типографика без материала</span>'}</div></article>`;}
function renderEditor() {
  const root=$('#editor');if(!state.spec){root.innerHTML=empty('↗','Соберём первый ролик','Начните с своей темы или демо. Дальше студия проведёт через сценарий, рендер и доставку.','<button class="button primary" data-open-draft>Новый ролик</button><button class="button secondary" data-example>Открыть демо</button>');return;}
  const p=state.editorProject,s=state.spec;
  root.innerHTML=`<div class="editor-heading"><div><span class="eyebrow">РЕДАКТОР РОЛИКА</span><h2>${esc(s.title)}</h2><p>Версия ${p.revision} · ${esc(niceDate(p.updated_at))}</p></div><span id="editor-status">${badge(p.status)}</span></div><div class="editor-body"><form id="edit-form" class="edit-form"><label>Название<input data-spec="title" value="${esc(s.title)}" maxlength="100" required></label><div class="form-grid"><label>Формат<select data-spec="profile">${profileOptions(s.profile)}</select></label><label>Голос<select data-spec="voice"><option value="none" ${s.voice==='none'?'selected':''}>Без голоса</option><option value="espeak" ${s.voice==='espeak'?'selected':''}>eSpeak · базовый</option><option value="piper" ${s.voice==='piper'?'selected':''}>Piper · нейросетевой</option></select></label></div><label>Описание<textarea data-spec="description" rows="2">${esc(s.description)}</textarea></label><div class="section-heading scene-heading"><h2>Сцены <span class="muted">/ ${s.scenes.length}</span></h2><button type="button" class="text-button" id="add-scene">+ Сцена</button></div><div id="scene-list">${s.scenes.map(sceneMarkup).join('')}</div><label>Музыка<div class="asset-row"><button type="button" class="button secondary" id="upload-music">+ Аудиофайл</button>${s.music_asset?`<code>${esc(s.music_asset)}</code><button class="text-button" id="clear-music" type="button">Убрать</button>`:'<span class="helper">Без музыки</span>'}</div></label>${s.source_urls.length?`<label>Источники</label><ul class="source-links">${s.source_urls.map(url=>`<li><a href="${esc(safeUrl(url))}" target="_blank" rel="noopener noreferrer">${esc(url)}</a></li>`).join('')}</ul>`:''}<details class="json-editor"><summary>Все поля проекта · JSON</summary><p class="helper">Тема, язык, теги, источники и параметры каждой сцены. Применение JSON меняет локальный черновик; затем сохраните.</p><textarea id="spec-json" rows="15" spellcheck="false" aria-label="Спецификация проекта JSON">${esc(state.pendingJson??JSON.stringify(s,null,2))}</textarea><button class="button secondary" type="button" id="apply-json">Применить JSON</button></details></form><div class="preview-panel" id="preview-panel"></div></div><div class="editor-save"><span id="dirty-label" class="dirty-label ${state.dirty?'dirty':''}">${state.dirty?'Есть несохранённые изменения':'Изменения сохранены'}</span><button class="button primary" id="save-project">Сохранить сценарий</button></div>`;
  updatePreview();
}
function updatePreview() {
  if(!state.spec || !$('#preview-panel')) return;
  const p=state.data?.projects.find(p=>p.id===state.selected) || state.editorProject,s=state.spec,a=p.artifact;const src=a?localPath(a.path,'renders'):null;
  const duration=s.scenes.reduce((n,scene)=>n+(Number(scene.duration_s)||0),0);const shape=s.profile==='landscape'?'landscape':s.profile==='square'?'square':'';
  const signature=JSON.stringify([state.selected,s.profile,duration,s.scenes.length,a?.id,a?.revision,p.revision,p.status,p.error,state.dirty]);
  $('#editor-status').innerHTML=badge(p.status);
  if($('#preview-panel').dataset.signature===signature)return;
  $('#preview-panel').dataset.signature=signature;
  const oldVideo=$('#preview-panel video'),oldSrc=oldVideo?.getAttribute('src'),playback=oldVideo?.currentTime || 0,wasPlaying=oldVideo && !oldVideo.paused;
  $('#preview-panel').innerHTML=`<div class="preview-shell"><div class="preview-title"><span>ПРЕДПРОСМОТР</span><span>${a?'v'+a.revision:'ЧЕРНОВИК'}</span></div><div class="video-frame ${shape}">${src?`<video src="${esc(src)}" controls preload="metadata" playsinline aria-label="Готовый ролик"></video>`:'<div class="video-placeholder"><span>▶</span><p>Готовое видео появится<br>после рендера</p></div>'}</div><div class="preview-meta"><span>${s.profile==='landscape'?'1920 × 1080':s.profile==='square'?'1080 × 1080':'1080 × 1920'}</span><span>${duration.toFixed(1).replace('.0','')} сек</span><span>${s.scenes.length} сцен</span></div><div class="preview-actions"><button class="button primary" id="render-project">Собрать ролик ↗</button><button class="button secondary" id="publish-project">Публикация / экспорт</button>${src?`<a class="button secondary" href="${esc(src)}" download>Скачать MP4 ↓</a>`:''}<button class="text-button" id="project-graph">Переходы этого проекта →</button></div><div class="preview-footer">${a&&(state.dirty||a.revision!==p.revision)?'Предпросмотр показывает предыдущий рендер. Сохраните изменения и соберите видео заново.':'Голос и монтаж выполняются на вашем компьютере.'}${p.error?`<div class="error-inline">${esc(p.error)}</div>`:''}</div></div>`;
  $('#editor-status').innerHTML=badge(p.status);
  if(src===oldSrc&&playback){const video=$('#preview-panel video');video.addEventListener('loadedmetadata',()=>{video.currentTime=Math.min(playback,video.duration||playback);if(wasPlaying)video.play().catch(()=>{});},{once:true});}
}
async function saveProject(button) {if(!state.spec) return;await withBusy(button,async()=>{
  if(state.pendingJson!==null)throw new Error('Сначала примените JSON к черновику.');
  if(!$('#edit-form').reportValidity()) return;
  const selected=state.selected,sent=JSON.stringify(state.spec);
  const response=await api('/api/projects/'+encodeURIComponent(selected),{method:'PUT',body:{expected_revision:state.editorProject.revision,spec:JSON.parse(sent)}});
  const project=response.project || response;
  if(state.selected===selected){const editedDuringSave=JSON.stringify(state.spec)!==sent;state.editorProject=clone(project);if(!editedDuringSave)state.spec=clone(project.spec);state.dirty=editedDuringSave;}
  toast('Сценарий сохранён · версия '+project.revision);await refreshState();if(state.selected===selected)renderEditor();
});}
async function createExample(button) {await withBusy(button,async()=>{if(state.dirty&&!confirm('Перейти к новому демо без сохранения текущих изменений?'))return;const spec=await api('/api/example');const response=await api('/api/projects',{method:'POST',body:spec.spec || spec});await refreshState();selectProject((response.project||response).id,false);showView('studio');toast('Демо готово к редактированию.');});}
function syncProjectSelects() {const options=(state.data?.projects || []).map(p=>`<option value="${esc(p.id)}">${esc(p.spec.title)}</option>`).join('');for(const el of $$('.project-select,#graph-project')){const previous=el.value;el.innerHTML=options || '<option value="">Нет проектов</option>';if(state.data?.projects.some(p=>p.id===previous))el.value=previous;else if(state.selected)el.value=state.selected;}}
function openDraft(trend) {const form=$('#draft-form');form.reset();state.draftDirty=false;if(trend){form.elements.topic.value=trend.title;form.elements.source_urls.value=trend.url;form.elements.angle.focus();}$('#draft-dialog').showModal();}
async function uploadAsset(index,button) {
  const input=document.createElement('input');input.type='file';input.accept=index==='music'?'audio/*,.wav,.mp3,.ogg,.flac':'image/*,video/*,.mp4,.webm,.mov';input.hidden=true;document.body.append(input);
  input.addEventListener('change',async()=>{try{if(!input.files?.[0])return;await withBusy(button,async()=>{const projectId=state.selected;const body=new FormData();body.append('file',input.files[0]);const result=await api('/api/assets',{method:'POST',body});if(state.selected!==projectId){toast('Материал загружен: '+result.asset);return;}if(index==='music')state.spec.music_asset=result.asset;else if(state.spec.scenes[index])state.spec.scenes[index].asset=result.asset;setDirty();renderEditor();toast('Материал добавлен. Сохраните сценарий.');});}finally{input.remove();}},{once:true});
  input.addEventListener('cancel',()=>input.remove(),{once:true});input.click();
}
function renderTrends() {const trends=state.data?.trends || [];$('#trend-count').textContent=trends.length+' сигналов';$('#trend-list').innerHTML=trends.length?trends.map(t=>`<article class="trend-card"><div class="trend-top"><span class="source-tag">${esc(t.source)}</span><span>${esc(niceDate(t.fetched_at))}</span></div><h3>${esc(t.title)}</h3><div class="trend-stats">${t.volume!=null?`${Number(t.volume).toLocaleString('ru-RU')} · `:''}${esc(t.evidence_kind)}<br><span class="helper">Публикация: ${esc(niceDate(t.published_at))}</span></div><div class="trend-bottom"><a href="${esc(safeUrl(t.url))}" target="_blank" rel="noopener noreferrer">Источник ↗</a><button class="button primary" data-trend="${esc(t.id)}">Свой сценарий +</button></div></article>`).join(''):empty('↗','Источники ещё не загружены','Выберите источник и регион, затем найдите сигналы. Здесь будут реальные результаты с ссылками.');}
function money(minor,currency='RUB') {if(minor==null)return 'Нет данных';if(typeof minor==='number'&&!Number.isSafeInteger(minor))return 'Сумма вне точности интерфейса';try {const n=BigInt(minor),negative=n<0n,a=negative?-n:n;return (negative?'−':'')+(a/100n).toLocaleString('ru-RU')+','+(a%100n).toString().padStart(2,'0')+' '+({RUB:'₽',USD:'$',EUR:'€'}[currency]||currency);}catch{return 'Некорректная сумма';}}
// Exact decimal -> integer minor units. No binary floating-point arithmetic.
function minorUnits(value,{optional=false,maximum=9000000000000}={}) {const s=String(value??'').trim().replace(',','.');if(!s&&optional)return null;if(!/^\d+(?:\.\d{1,2})?$/.test(s))throw new Error('Введите неотрицательную сумму с точностью до двух знаков.');const [whole,fraction='']=s.split('.');const amount=BigInt(whole)*100n+BigInt(fraction.padEnd(2,'0'));if(amount>BigInt(maximum))throw new Error('Сумма превышает допустимый размер.');return Number(amount);}
function integer(value) {const s=String(value).trim();if(!/^\d+$/.test(s))throw new Error('Просмотры: требуется целое неотрицательное число.');const n=BigInt(s);if(n>9000000000000n)throw new Error('Число просмотров слишком большое.');return Number(n);}
function renderMoney() {
  const totals=state.data?.totals || [],rows=state.data?.ledger || [];
  $('#money-totals').innerHTML=totals.length?totals.map(t=>`<article class="money-card"><h3><span>${esc(t.currency)}</span><span>${Number(t.views).toLocaleString('ru-RU')} просмотров</span></h3><div class="money-label">Прогноз ${t.forecast_complete?'':'· известная часть'}</div><div class="money-number">${esc(money(t.forecast_minor_known,t.currency))}</div><div class="helper">${t.forecast_complete?'По RPM и доле просмотров либо оценке платформы':`У ${t.unknown_forecast_rows} записей нет данных для прогноза`}</div><div class="money-actual"><span>Подтверждённая выручка</span><strong>${t.actual_rows?esc(money(t.actual_revenue_minor,t.currency)):'Нет данных'}</strong></div><div class="money-detail"><span>Расходы</span><span>${esc(money(t.cost_minor,t.currency))}</span></div><div class="money-detail"><span>Прогноз прибыли</span><span>${t.forecast_profit_minor==null?'Неполные данные':esc(money(t.forecast_profit_minor,t.currency))}</span></div><div class="money-detail"><span>Выручка минус расходы</span><span>${t.actual_rows?esc(money(t.actual_profit_minor,t.currency)):'Нет данных'}</span></div></article>`).join(''):empty('◉','Пока нет показателей','Добавьте просмотры, расходы и RPM либо импортируйте данные своего канала.');
  $('#ledger-table').innerHTML=rows.length?`<table><thead><tr><th>Проект / платформа</th><th>Дата</th><th>Просмотры</th><th>RPM</th><th>Доля / монетизация</th><th>Факт</th><th>Оценка API</th><th>Расходы</th></tr></thead><tbody>${rows.map(r=>`<tr><td>${esc(projectTitle(r.project_id))}<small>${esc(r.platform)} · ${esc(r.source)}</small></td><td>${esc(r.date)}</td><td>${Number(r.views).toLocaleString('ru-RU')}</td><td>${esc(money(r.rpm_minor,r.currency))}</td><td>${(r.eligible_bps/100).toFixed(2).replace(/\.00$/,'')}% · ${r.monetized?'да':'нет'}</td><td>${esc(money(r.actual_revenue_minor,r.currency))}</td><td>${esc(money(r.api_estimated_revenue_minor,r.currency))}</td><td>${esc(money(r.cost_minor,r.currency))}</td></tr>`).join('')}</tbody></table>`:empty('≋','Журнал пуст','Внесите первую запись после публикации ролика.');
  $('#ledger-table').insertAdjacentHTML('beforeend','<div class="csv-help">Колонки CSV: <code>project_id,platform,date,views,currency,rpm_minor,monetized,actual_revenue_minor,api_estimated_revenue_minor,cost_minor,eligible_bps,source</code>. Суммы в минимальных единицах валюты (например, 12050 = 120,50). Неизвестные суммы оставьте пустыми.</div>');
}
function resultLinks(value) {const links=new Set();function visit(v){if(typeof v==='string'&&/^(?:\/)?exports\//.test(v)&&/\/[^/]+\.[a-z0-9]{1,8}$/i.test(v)){const href=localPath(v.replace(/^\//,''),'exports');if(href)links.add(href);}else if(Array.isArray(v))v.forEach(visit);else if(v&&typeof v==='object')Object.values(v).forEach(visit);}visit(value);return [...links];}
const pipelineDefaults={source:'google',region:'RU',angle:'',profile:'shorts',voice:'espeak',language:'ru',use_ollama:false,platform:'export',privacy:'private',dry_run:true,limit:1,daily_budget:2,interval_minutes:60,utc_offset_hours:3};
function pipelineFormFingerprint() {return JSON.stringify([...new FormData($('#pipeline-form')).entries()]);}
function fillPipelineForm(config,enabled) {
  const form=$('#pipeline-form');const settings={...pipelineDefaults,...(config || {})};
  for(const [key,value] of Object.entries(settings)){const control=form.elements.namedItem(key);if(!control)continue;if(control.type==='checkbox')control.checked=Boolean(value);else control.value=String(value);}
  form.elements.enabled.checked=Boolean(enabled);state.pipelineLoaded=true;
}
function pipelineDirtyLabel() {$('#pipeline-dirty').textContent=state.pipelineDirty?'Есть несохранённые настройки':'Настройки сохранены';$('#pipeline-dirty').className=state.pipelineDirty?'dirty-label dirty':'muted';}
function readPipelineConfig() {
  const form=$('#pipeline-form');const f=new FormData(form);const config={source:f.get('source'),region:f.get('region'),angle:String(f.get('angle') || '').trim(),profile:f.get('profile'),voice:f.get('voice'),language:f.get('language'),use_ollama:f.get('use_ollama')==='true',platform:f.get('platform'),privacy:f.get('privacy'),dry_run:f.has('dry_run')};
  if(!config.angle||config.angle.length>240)throw new Error('Задайте свой оригинальный угол: от 1 до 240 символов.');
  for(const [key,min,max] of [['limit',1,3],['daily_budget',1,10],['interval_minutes',15,1440],['utc_offset_hours',-12,14]]){const value=Number(f.get(key));if(!Number.isInteger(value)||value<min||value>max)throw new Error(`Некорректный параметр ${key}: ${min}–${max}.`);config[key]=value;}
  return config;
}
function reportProjects(report) {
  const ids=new Set(),known=new Set((state.data?.projects || []).map(p=>p.id));
  function visit(value){if(typeof value==='string'&&known.has(value))ids.add(value);else if(Array.isArray(value))value.forEach(visit);else if(value&&typeof value==='object')Object.values(value).forEach(visit);}
  visit(report);return [...ids].map(id=>state.data.projects.find(p=>p.id===id)).filter(Boolean).sort((a,b)=>new Date(b.created_at)-new Date(a.created_at));
}
function renderPipeline() {
  const pipeline=state.data?.pipeline,summary=$('#pipeline-summary'),report=$('#pipeline-report');
  if(!pipeline){summary.innerHTML=empty('⟳','Конвейер недоступен','Обновите и перезапустите студию, чтобы использовать автоматические циклы.');report.innerHTML='';$('#pipeline-run').disabled=true;$('#pipeline-save').disabled=true;return;}
  $('#pipeline-run').disabled=pipeline.status==='running'||Boolean(state.pipelineBusy);$('#pipeline-save').disabled=Boolean(state.pipelineBusy);
  if(!state.pipelineLoaded||!state.pipelineDirty)fillPipelineForm(pipeline.config,pipeline.enabled);
  pipelineDirtyLabel();
  const config=pipeline.status==='running'?(pipeline.active_config || null):pipeline.config,current=pipeline.status==='running'?'В работе':pipeline.enabled?'Ожидает цикла':'Выключен';
  summary.innerHTML=`<div class="section-heading"><h2>Состояние</h2><span class="status ${pipeline.status==='running'?'running':pipeline.enabled?'ready':''}">${current}</span></div><div class="pipeline-status-mark ${pipeline.status==='running'?'working':''}" aria-hidden="true">⟳</div><h3>${pipeline.status==='running'?'Студия собирает ролики':pipeline.enabled?'Повторные циклы включены':'Готов к первому запуску'}</h3><p class="helper">${pipeline.status==='running'?'Рендер использует общий ресурс компьютера. Ручные задания могут ждать завершения цикла.':pipeline.enabled?`Интервал: ${Number(pipeline.config?.interval_minutes || 60)} мин · максимум ${Number(pipeline.config?.daily_budget || 2)} роликов в день.`:'Запустите один цикл, чтобы проверить результат, или явно включите расписание в настройках.'}</p>${config?`<div class="pipeline-setting-pills"><span>${esc(config.platform)}</span><span>${esc(profiles[config.profile] || config.profile)}</span><span>${config.dry_run?'Проверочный режим':'Отправка включена'}</span></div>`:''}${pipeline.status==='running'&&pipeline.config?`<p class="helper">Сохранённая политика расписания: ${esc(pipeline.config.platform)} · ${pipeline.config.dry_run?'проверочный режим':'отправка включена'} · ${Number(pipeline.config.interval_minutes)} мин.</p>`:''}${pipeline.enabled?'<button class="button danger" id="pipeline-off" type="button">Выключить расписание</button>':''}<p class="helper">Выключение расписания останавливает будущие циклы; уже начатые задания остаются в очереди.</p>${pipeline.last_error?`<div class="error-inline">${esc(pipeline.last_error)}</div>`:''}`;
  const projects=reportProjects(pipeline.last_report);
  report.innerHTML=`<div class="section-heading"><h2>Последний цикл</h2><button class="text-button" data-view="jobs">Очередь →</button></div>${pipeline.last_report?`${projects.length?'<div class="pipeline-created">'+projects.map(p=>`<button type="button" class="pipeline-project-link" data-job-project="${esc(p.id)}"><span>${esc(p.spec.title)}</span><small>${esc(p.status)} · v${p.revision} →</small></button>`).join('')+'</div>':'<p class="helper">Отчёт сохранён; подробности ниже.</p>'}<details><summary>Отчёт цикла и основания</summary><pre>${esc(JSON.stringify(pipeline.last_report,null,2))}</pre></details>`:'<p class="helper">После первого запуска здесь появятся созданные проекты и отчёт. Все переходы доступны в графе.</p>'}`;
}

function renderJobs() {const jobs=state.data?.jobs || [];$('#job-list').innerHTML=jobs.length?[...jobs].reverse().map(j=>`<article class="job-card"><div class="job-icon" aria-hidden="true">${j.kind==='render'?'▶':'↗'}</div><div class="job-main"><h3>${esc(projectTitle(j.project_id))}</h3><p>${esc(labels[j.kind]||j.kind)} · ${esc(niceDate(j.created_at))}${j.due_at?' · запуск '+esc(niceDate(j.due_at)):''}</p><div class="job-meta">${badge(j.state)}<span class="muted">${Math.round(Math.max(0,Math.min(1,Number(j.progress)||0))*100)}%</span>${j.request?.publish?.dry_run?'<span class="status">Проверочный запуск</span>':''}</div>${activeStates.has(j.state)?`<div class="job-progress"><span style="width:${Math.round(Math.max(0,Math.min(1,Number(j.progress)||0))*100)}%"></span></div>`:''}${j.error?`<p class="job-error">${esc(j.error)}</p>`:''}${resultLinks(j.result).length?`<div class="job-links">${resultLinks(j.result).map(href=>`<a class="button secondary" href="${esc(href)}" download>Скачать ${esc(decodeURIComponent(href.split('/').at(-1)))}</a>`).join('')}</div>`:''}${j.result?`<details><summary>Результат задания</summary><pre>${esc(JSON.stringify(j.result,null,2))}</pre></details>`:''}<details><summary>Параметры и идентификатор</summary><pre>${esc(JSON.stringify({id:j.id,operation_key:j.operation_key,request:j.request},null,2))}</pre></details></div><div class="job-side">${j.kind==='publish'&&activeStates.has(j.state)?`<button class="button danger" data-cancel-job="${esc(j.id)}">Отменить</button>`:''}<button class="text-button" data-job-project="${esc(j.project_id)}">К проекту →</button></div></article>`).join(''):empty('≋','Очередь свободна','Соберите ролик или создайте задание на экспорт. Каждый запуск появится здесь.');}
async function loadGraph() {
  const request=++state.graphRequest,id=$('#graph-project').value;
  if(!id){$('#graph-canvas').innerHTML=empty('⌘','Нет событий','Создайте проект, чтобы появились переходы.');$('#graph-snapshot').innerHTML='';return;}
  let path='/api/graph?project_id='+encodeURIComponent(id),snapshot='/api/projects/'+encodeURIComponent(id)+'/snapshot';const date=$('#graph-as-of').value;
  if(date){const d=new Date(date);if(Number.isNaN(d.getTime())){toast('Укажите корректное время среза.',true);return;}const time=encodeURIComponent(d.toISOString());path+='&as_of='+time;snapshot+='?as_of='+time;}
  const [graph,snap]=await Promise.allSettled([api(path),api(snapshot)]);if(request!==state.graphRequest)return;
  if(graph.status==='rejected'){toast(graph.reason.message,true);return;}
  state.graph=graph.value;state.snapshot=snap.status==='fulfilled'?snap.value.project:null;state.snapshotError=snap.status==='rejected'?snap.reason.message:null;
  if(state.graphEvent&&!state.graph.nodes.some(n=>n.id===state.graphEvent))state.graphEvent=null;renderGraph();
}
function relationKind(relation) {const r=String(relation||'').toLowerCase();if(/hypoth|cause_candidate|possible/.test(r))return 'hypothesis';if(/chron|temporal|sequence|previous|record/.test(r))return 'chronology';return 'dependency';}
function renderGraph() {
  const graph=state.graph;if(!graph)return;const integrity=graph.integrity || {},clock=$('#graph-clock').value;
  $('#graph-integrity').textContent=integrity.valid?'Хеш-цепочка цела':'Ошибка целостности';$('#graph-integrity').className='status '+(integrity.valid?'ready':'error');$('#graph-integrity').title=integrity.error||`${integrity.count || 0} событий · ${integrity.head_hash || ''}`;
  $('#graph-warning').hidden=graph.boundary_complete!==false;
  $('#graph-warning').textContent='Срез исключил часть сведений о родительских событиях. Граница доказательств неполна; этот вид графа не подтверждает весь переход.';
  const snapshot=state.snapshot;
  $('#graph-snapshot').innerHTML=state.snapshotError?`<div class="error-inline">Исторический снимок недоступен: ${esc(state.snapshotError)}</div>`:snapshot?`<details><summary>Состояние в этом срезе · v${snapshot.revision} · ${esc(snapshot.spec.title)}</summary><p class="helper">Снимок только для чтения. Открытие не повторяет рендер или публикацию.</p><pre>${esc(JSON.stringify(snapshot,null,2))}</pre></details>`:'<p class="helper">В этом срезе ещё нет сохранённого состояния проекта.</p>';
  const nodes=[...graph.nodes].sort((a,b)=>new Date(a[clock])-new Date(b[clock])||a.seq-b.seq);
  if(!nodes.length){$('#graph-canvas').innerHTML=empty('⌘','В этом срезе нет событий','Выберите другое время или вернитесь к текущему состоянию.');renderInspector(null);return;}
  const scopes=[...new Set(nodes.map(n=>n.spatial_scope || n.subject_id))];const width=Math.max(660,nodes.length*177+160),height=Math.max(300,scopes.length*145+95);
  const positions=new Map(nodes.map((n,i)=>[n.id,{x:155+i*177,y:63+scopes.indexOf(n.spatial_scope||n.subject_id)*145}]));
  const edgeMarkup=(graph.edges || []).map(e=>{const a=positions.get(e.source),b=positions.get(e.target);if(!a||!b)return '';const k=relationKind(e.relation),color={dependency:'#6f9568',hypothesis:'#d58363',chronology:'#aab5ac'}[k],dash=k==='hypothesis'?'7 5':k==='chronology'?'2 5':'';const ax=a.x+129,ay=a.y+34,bx=b.x,by=b.y+34;return `<path d="M ${ax} ${ay} C ${ax+24} ${ay}, ${bx-24} ${by}, ${bx} ${by}" fill="none" stroke="${color}" stroke-width="1.6" ${dash?`stroke-dasharray="${dash}"`:''} marker-end="url(#arrow-${k})"><title>${esc(e.relation)}</title></path>`;}).join('');
  $('#graph-canvas').innerHTML=`<svg width="${width}" height="${height}" role="img" aria-label="События проекта по времени, связанные зависимостями, гипотезами и последовательностью"><defs>${['dependency','hypothesis','chronology'].map(k=>`<marker id="arrow-${k}" viewBox="0 0 6 6" refX="5" refY="3" markerWidth="5" markerHeight="5" orient="auto"><path d="M0,0 L6,3 L0,6" fill="${{dependency:'#6f9568',hypothesis:'#d58363',chronology:'#aab5ac'}[k]}"/></marker>`).join('')}</defs>${scopes.map((scope,i)=>`<line x1="145" y1="${97+i*145}" x2="${width-25}" y2="${97+i*145}" stroke="#e3eadf" stroke-dasharray="3 5"/><text x="13" y="${94+i*145}" font-size="9" fill="#758b70"><title>${esc(scope)}</title>${esc(short(scope,22))}</text>`).join('')}${edgeMarkup}${nodes.map(n=>{const p=positions.get(n.id);return `<g class="event-node ${state.graphEvent===n.id?'selected':''}" data-event="${esc(n.id)}" transform="translate(${p.x},${p.y})" tabindex="0" role="button" aria-label="Событие ${n.seq}: ${esc(n.from_state)} → ${esc(n.to_state)}"><rect width="129" height="71" rx="10"/><text x="10" y="18" font-size="8" fill="#83917b">#${n.seq} · ${esc(short(n.claim_level,18))}</text><text x="10" y="35" font-size="10" font-weight="600" fill="#233d2c">${esc(short(n.to_state,18))}</text><text x="10" y="54" font-size="8" fill="#7a8972">${esc(short(n.reason,23))}</text><text x="4" y="87" font-size="8" fill="#829079">${esc(niceDate(n[clock],true))}</text><title>${esc(n.from_state+' → '+n.to_state+' · '+n.reason)}</title></g>`;}).join('')}<text x="155" y="${height-15}" font-size="9" fill="#90a087">Порядок по ${clock==='valid_at'?'времени события':'времени записи'} · интервалы между узлами равномерные</text></svg>`;
  renderInspector(nodes.find(n=>n.id===state.graphEvent) || null);
}
function renderInspector(event) {const root=$('#event-inspector');if(!event){root.innerHTML=empty('⌘','Выберите событие','Проверьте причину, ожидание, наблюдение и доказательства.');return;}root.innerHTML=`<span class="eyebrow">СОБЫТИЕ #${event.seq}</span><h3>${esc(event.from_state)} → ${esc(event.to_state)}</h3>${badge(event.claim_level)}<dl><dt>Основание перехода</dt><dd>${esc(event.reason)}</dd><dt>Пространство</dt><dd>${event.space?`<pre>${esc(JSON.stringify(event.space,null,2))}</pre>`:esc(event.spatial_scope)}</dd><dt>Когда произошло</dt><dd>${esc(niceDate(event.valid_at,true))}</dd><dt>Когда записано</dt><dd>${esc(niceDate(event.recorded_at,true))}</dd>${event.confidence!=null?`<dt>Заявленная уверенность</dt><dd>${esc(event.confidence)}</dd>`:''}<dt>Ожидание</dt><dd><pre>${esc(JSON.stringify(event.expected,null,2))}</pre></dd><dt>Наблюдение</dt><dd><pre>${esc(JSON.stringify(event.observed,null,2))}</pre></dd><dt>Доказательства</dt><dd><pre>${esc(JSON.stringify(event.evidence,null,2))}</pre></dd><dt>Родительские события / тип связи</dt><dd>${event.parents.length?event.parents.map(p=>`<div>${esc(p.relation)}<br><code>${esc(p.id)}</code><br><small>${esc(p.hash)}</small></div>`).join('<br>'):'Нет родителей'}</dd></dl><details><summary>Хеш и идентификаторы</summary><pre>${esc(JSON.stringify({id:event.id,subject_id:event.subject_id,operation_id:event.operation_id,execution_id:event.execution_id,supersedes:event.supersedes,previous_hash:event.previous_hash,hash:event.hash},null,2))}</pre></details>`;}

document.addEventListener('click',event=>{
  const el=event.target.closest('button,a,[data-event]');if(!el)return;
  if(el.dataset.view)showView(el.dataset.view);
  if(el.dataset.project)selectProject(el.dataset.project);
  if(el.classList.contains('close-dialog'))el.closest('dialog')?.close();
  if(el.matches('#new-project,[data-open-draft]'))openDraft();
  if(el.matches('#add-example,[data-example]'))createExample(el);
  if(el.id==='refresh-state')withBusy(el,()=>refreshState());
  if(el.id==='save-project')saveProject(el);
  if(el.id==='add-scene'){if(state.spec.scenes.length>=20){toast('Максимум 20 сцен.',true);return;}state.spec.scenes.push({title:'Новая сцена',text:'',narration:'',duration_s:5,asset:null});setDirty();renderEditor();}
  if(el.dataset.removeScene!==undefined){if(state.spec.scenes.length===1){toast('В проекте должна остаться хотя бы одна сцена.',true);return;}state.spec.scenes.splice(Number(el.dataset.removeScene),1);setDirty();renderEditor();}
  if(el.dataset.clearScene!==undefined){state.spec.scenes[Number(el.dataset.clearScene)].asset=null;setDirty();renderEditor();}
  if(el.dataset.uploadScene!==undefined)uploadAsset(Number(el.dataset.uploadScene),el);
  if(el.id==='upload-music')uploadAsset('music',el);
  if(el.id==='clear-music'){state.spec.music_asset=null;setDirty();renderEditor();}
  if(el.id==='apply-json'){try{const value=JSON.parse($('#spec-json').value);if(!value||typeof value!=='object'||typeof value.title!=='string'||!profiles[value.profile]||!Array.isArray(value.scenes)||!value.scenes.length||value.scenes.length>20||!Array.isArray(value.source_urls)||!value.source_urls.every(u=>typeof u==='string')||!value.scenes.every(s=>s&&typeof s.title==='string'&&typeof s.text==='string'&&typeof s.narration==='string'&&typeof s.duration_s==='number'))throw new Error('Проверьте структуру: title, profile, scenes (1–20 объектов), source_urls (массив строк).');state.spec=value;state.pendingJson=null;setDirty();renderEditor();toast('JSON применён к черновику.');}catch(e){toast(e.message,true);}}
  if(el.id==='render-project')withBusy(el,async()=>{if(state.dirty){toast('Сначала сохраните изменения сценария.',true);return;}await api('/api/projects/'+encodeURIComponent(state.selected)+'/render',{method:'POST',body:{}});toast('Рендер поставлен в очередь.');await refreshState();showView('jobs');});
  if(el.id==='publish-project'){if(state.dirty){toast('Сохраните сценарий перед созданием задания.',true);return;}$('#publish-form').reset();$('#publish-project-name').textContent=state.spec.title;$('#publish-dialog').dataset.project=state.selected;$('#publish-dialog').showModal();}
  if(el.id==='project-graph'){showView('graph');}
  if(el.dataset.trend){const trend=state.data.trends.find(t=>t.id===el.dataset.trend);if(trend)openDraft(trend);}
  if(el.dataset.event){state.graphEvent=el.dataset.event;renderGraph();}
  if(el.id==='graph-current'){$('#graph-as-of').value='';loadGraph();}
  if(el.dataset.cancelJob)withBusy(el,async()=>{await api('/api/jobs/'+encodeURIComponent(el.dataset.cancelJob)+'/cancel',{method:'POST',body:{}});toast('Отмена запрошена.');await refreshState();});
  if(el.dataset.jobProject){selectProject(el.dataset.jobProject);if(state.selected===el.dataset.jobProject)showView('studio');}
  if(el.id==='new-ledger'){if(!state.data?.projects.length){toast('Сначала создайте проект.',true);return;}$('#ledger-form').reset();syncProjectSelects();$('#ledger-form').elements.date.value=new Date().toLocaleDateString('sv-SE');$('#ledger-dialog').showModal();}
  if(el.id==='import-csv')$('#csv-file').click();
  if(el.id==='pipeline-off')withBusy(el,async()=>{const pipeline=state.data?.pipeline;if(!pipeline?.config)throw new Error('Сохранённые настройки отсутствуют.');await api('/api/pipeline/config',{method:'POST',body:{enabled:false,config:pipeline.config}});$('#pipeline-form').elements.enabled.checked=false;await refreshState();toast('Расписание выключено.');});
  if(el.id==='analytics-button'){if(!state.data?.projects.length){toast('Сначала создайте и опубликуйте проект.',true);return;}$('#analytics-form').reset();syncProjectSelects();const today=new Date().toLocaleDateString('sv-SE');$('#analytics-form').elements.start.value=today;$('#analytics-form').elements.end.value=today;$('#analytics-dialog').showModal();}
});
document.addEventListener('input',event=>{const el=event.target;if(el.closest('#draft-form'))state.draftDirty=true;if(el.closest('#pipeline-form')){state.pipelineDirty=true;pipelineDirtyLabel();}if(el.id==='spec-json'){state.pendingJson=el.value;setDirty();$('#dirty-label').textContent='Примените JSON, затем сохраните';}if(el.dataset.spec){state.spec[el.dataset.spec]=el.value;setDirty();}if(el.dataset.sceneField){const scene=state.spec.scenes[Number(el.closest('[data-scene]').dataset.scene)],key=el.dataset.sceneField;scene[key]=key==='duration_s'?Number(el.value):el.value;setDirty();}});
document.addEventListener('keydown',event=>{if(event.target.matches('[data-event]')&&['Enter',' '].includes(event.key)){event.preventDefault();state.graphEvent=event.target.dataset.event;renderGraph();}});
$('#draft-form').addEventListener('submit',event=>{event.preventDefault();const form=event.currentTarget;withBusy($('[type="submit"]',form),async()=>{if(state.dirty&&!confirm('Перейти к новому проекту без сохранения текущих изменений?'))return;const f=new FormData(form);const body=Object.fromEntries(f);body.source_urls=String(body.source_urls).split(/\r?\n/).map(s=>s.trim()).filter(Boolean);body.use_ollama=body.use_ollama==='true';const result=await api('/api/draft',{method:'POST',body});form.closest('dialog').close();await refreshState();selectProject(result.project.id,false);showView('studio');toast('Сценарий создан · '+result.generator);});});
$('#trend-form').addEventListener('submit',event=>{event.preventDefault();const form=event.currentTarget;withBusy($('[type="submit"]',form),async()=>{const body=Object.fromEntries(new FormData(form));await api('/api/trends/refresh',{method:'POST',body});await refreshState();toast('Сигналы обновлены.');});});
$('#publish-form').addEventListener('submit',event=>{event.preventDefault();const form=event.currentTarget;withBusy($('[type="submit"]',form),async()=>{const f=new FormData(form),scheduled=f.get('scheduled_at');const body={platform:f.get('platform'),privacy:f.get('privacy'),dry_run:f.has('dry_run'),scheduled_at:scheduled?new Date(scheduled).toISOString():null,made_for_kids:f.has('made_for_kids'),contains_synthetic_media:f.has('contains_synthetic_media')};if(!body.dry_run&&body.platform!=='export'&&!confirm(`Отправить ролик в ${body.platform==='youtube'?'YouTube':'Telegram'}${body.scheduled_at?' по расписанию':' сейчас'}?`))return;await api('/api/projects/'+encodeURIComponent(form.closest('dialog').dataset.project)+'/publish',{method:'POST',body});form.closest('dialog').close();await refreshState();showView('jobs');toast('Задание создано.');});});
$('#ledger-form').addEventListener('submit',event=>{event.preventDefault();const form=event.currentTarget;withBusy($('[type="submit"]',form),async()=>{const f=new FormData(form);const body={project_id:f.get('project_id'),platform:f.get('platform'),date:f.get('date'),views:integer(f.get('views')),currency:f.get('currency'),rpm_minor:minorUnits(f.get('rpm'),{optional:true}),monetized:f.has('monetized'),actual_revenue_minor:minorUnits(f.get('actual'),{optional:true}),api_estimated_revenue_minor:null,cost_minor:minorUnits(f.get('cost')),eligible_bps:minorUnits(f.get('eligible'),{maximum:10000}),source:'manual'};await api('/api/ledger',{method:'POST',body});form.closest('dialog').close();await refreshState();toast('Показатели сохранены.');});});
$('#csv-file').addEventListener('change',async event=>{const input=event.currentTarget;if(!input.files?.[0])return;await withBusy($('#import-csv'),async()=>{const body=new FormData();body.append('file',input.files[0]);const result=await api('/api/ledger/import',{method:'POST',body});await refreshState();toast('CSV импортирован'+(result.rows_imported!=null?': '+result.rows_imported+' строк':''));});input.value='';});
$('#analytics-form').addEventListener('submit',event=>{event.preventDefault();const form=event.currentTarget;withBusy($('[type="submit"]',form),async()=>{const f=new FormData(form);if(f.get('start')>f.get('end'))throw new Error('Начало периода должно быть раньше окончания.');const result=await api('/api/projects/'+encodeURIComponent(f.get('project_id'))+'/analytics',{method:'POST',body:{start:f.get('start'),end:f.get('end'),currency:f.get('currency'),include_revenue:f.has('include_revenue')}});form.closest('dialog').close();await refreshState();toast('Импортировано строк: '+result.rows_imported);});});
$('#pipeline-form').addEventListener('submit',event=>{
  event.preventDefault();const form=event.currentTarget,button=event.submitter || $('#pipeline-save');if(state.pipelineBusy)return;
  state.pipelineBusy=true;
  withBusy(button,async()=>{
    const config=readPipelineConfig(),fingerprint=pipelineFormFingerprint();
    if(button.value==='run'){
      const result=await api('/api/pipeline/run',{method:'POST',body:config});
      toast('Цикл запущен'+(result.run_id?' · '+short(result.run_id,12):'')+'. Расписание не меняется.');
    }else{
      const enabled=form.elements.enabled.checked;
      await api('/api/pipeline/config',{method:'POST',body:{enabled,config}});
      if(pipelineFormFingerprint()===fingerprint)state.pipelineDirty=false;
      toast(enabled?'Настройки сохранены. Повторные циклы включены.':'Настройки сохранены. Расписание выключено.');
    }
    await refreshState();
  }).finally(()=>{state.pipelineBusy=false;renderPipeline();});
});
$('#graph-form').addEventListener('submit',event=>{event.preventDefault();loadGraph();});
$('#graph-project').addEventListener('change',()=>{state.graphEvent=null;loadGraph();});
$('#graph-clock').addEventListener('change',renderGraph);
$('#hypothesis-form').addEventListener('submit',event=>{event.preventDefault();const form=event.currentTarget;withBusy($('[type="submit"]',form),async()=>{const id=$('#graph-project').value;if(!id)throw new Error('Сначала выберите проект.');const result=await api('/api/projects/'+encodeURIComponent(id)+'/hypothesis',{method:'POST',body:{reason:form.elements.reason.value.trim()}});form.reset();$('#graph-as-of').value='';state.graphEvent=result.id;await loadGraph();toast('Гипотеза записана. Её влияние ещё предстоит проверить.');});});
window.addEventListener('beforeunload',event=>{if(state.dirty||state.pipelineDirty){event.preventDefault();event.returnValue='';}});
window.addEventListener('online',()=>refreshState());
showView('studio');refreshState();
