// The package's production close adapter is supplied by the Rust test, together
// with the current runtime. Native cancellation rejects a pending raw pull;
// resolving it with a fake done step would miss the owner-close regression.
async function checkOwnedStreamClose() {
  const watchdog=setTimeout(()=>{throw Error('owner-close adapter regression timed out');},10000);
  try {
  const {BackendSession,Host,createFacade,invokeObjectOperation}=await import(
    'data:text/javascript;base64,'+Buffer.from(runtimeSource).toString('base64'));
  const construct=10000,ownerClose=10001;
  const named={kind:'named',typeId:0,name:'Client'};
  const operations={
    [construct]:{id:construct,name:'create',kind:'Constructor',surface:'object',ownerTypeId:0,args:[],returnType:named},
    [ownerClose]:{id:ownerClose,name:'close',receiver:true,receiverTypeId:0,args:[],async:true,resourceClose:true},
    [factory]:{id:factory,name:'events',receiver:true,receiverTypeId:0,args:[],returnType:{kind:'outputStream',inner:{kind:'scalar',name:'I32'}},
      streamResources:[{direction:'Output',path:'return',item:{kind:'scalar',name:'I32'},error:null,slots:{OutputStreamNext:pull,OutputStreamCancel:cancel}}]},
  };
  class Client {
    close() {return invokeObjectOperation(this,operations[ownerClose]);}
    events() {return invokeObjectOperation(this,operations[factory]);}
  }
  const descriptors={componentId:0,operations,types:{0:{kind:'object'}},classes:{0:{typeName:'Client',typeKey:'stream-owner.Client',public:true,publicClass:Client,resource:true}}};
  for(const engine of ['wasm','napi']) for(const scenario of [
    'owner','cancel-before-owner','owner-before-cancel','unrelated-cancel',
    'cleanup-failure','cleanup-cancelled','transport-failure','protocol-failure','business-cancelled',
  ]) {
    const raw=deferred(),cleanup=deferred(),rawFrame=deferred(),native=deferred();
    const carrier={handle:1};
    let cancellations=0,releases=0,extraAcks=0,closeDone=false,readDone=false;
    const error=(errorName)=>Object.assign(Error('structured '+engine+' error'),{errorName,domain:engine==='napi'?'backend':null});
    const interruption=error('UniffiCancelled');
    const failure=scenario==='cleanup-failure'?error('CleanupProbe'):scenario==='cleanup-cancelled'?error('UniffiCancelled'):null;
    const pullFailure=scenario==='transport-failure'?error('TransportProbe'):scenario==='protocol-failure'?error('UniffiStreamProtocol'):interruption;
    const wrapped=vm.runInNewContext(source+'; __withStreamClose(backend)',{Promise,backend:{
      invokeSync(id) {if(id===construct)return {kind:'value',value:9000};assert.equal(id,factory);return {kind:'value',value:carrier};},
      invokeAsync(id) {
        if(id===ownerClose)return native.promise;
        if(id===pull)return raw.promise;
        if(id===ack){extraAcks++;return Promise.resolve({kind:'value',value:1});}
        throw Error('unexpected operation '+id);
      },
      cancelOutputStream(handle) {
        assert.equal(handle,carrier);cancellations++;
        return Promise.all([cleanup.promise,rawFrame.promise]).then(()=>{
          if(scenario==='business-cancelled') raw.resolve({kind:'value',value:{kind:'error',error:interruption}});
          else raw.reject(pullFailure);
          if(failure)throw failure;
        });
      },
      releaseOutputStream(handle) {assert.equal(handle,carrier);releases++;},
      releaseObject() {},close() {},
    }});
    const session=new BackendSession(wrapped,new Host(),{callControl:{create:ack-3,cancel:ack-2,release:ack-1,operations:[]}});
    const owner=createFacade(session,descriptors).Client.create(),stream=owner.events();
    const reading=stream.next().then(value=>({value}),error=>({error})).then(value=>{readDone=true;return value;});
    await tick();
    if(scenario==='unrelated-cancel') {
      raw.reject(interruption);
      const result=await reading;
      assert.equal(result.error?.errorName,'UniffiCancelled','raw cancellation outside owner close remains an error');
      native.resolve();await owner.close();await session.close();continue;
    }
    const explicit=scenario==='cancel-before-owner'?stream.cancel():null;
    const closing=owner.close().then(value=>{closeDone=true;return {value};},error=>{closeDone=true;return {error};});
    await tick();
    if(scenario==='owner-before-cancel') stream.cancel();
    assert.equal(cancellations,1);assert.equal(readDone,false);assert.equal(closeDone,false);
    cleanup.resolve();await tick();
    assert.equal(readDone,false,'native pull frame is still active');assert.equal(releases,0);
    rawFrame.resolve();await tick();
    assert.equal(closeDone,false,'owner close must still join native cleanup');
    native.resolve();
    const closeResult=await closing,result=await reading;
    if(failure) {
      assert.equal(closeResult.error?.errorName,failure.errorName);
      assert.equal(result.error?.errorName,failure.errorName,'cleanup failure cannot become EOF');
      await assert.rejects(stream.next(),{errorName:failure.errorName});
    } else {
      assert.ok(!closeResult.error);
      const expected=scenario==='cancel-before-owner'||scenario==='business-cancelled'?'UniffiCancelled'
        :scenario==='transport-failure'?'TransportProbe':scenario==='protocol-failure'?'UniffiStreamProtocol':null;
      if(expected) assert.equal(result.error?.errorName,expected,engine+'/'+scenario);
      else assert.deepEqual(result,{value:{done:true,value:undefined}},engine+'/'+scenario);
      await explicit;
      for(let i=0;i<2;i++)assert.deepEqual(await stream.next(),{done:true,value:undefined});
      assert.equal(releases,1);assert.equal(extraAcks,0);
      await session.close();
    }
  }
  } finally {clearTimeout(watchdog);}
}
