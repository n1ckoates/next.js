/* __next_internal_action_entry_do_not_use__ {"6a88810ecce4a4e8b59d53b8327d7e98bbf251d7":"$$RSC_SERVER_ACTION_0","90b5db271335765a4b0eab01f044b381b5ebd5cd":"$$RSC_SERVER_ACTION_1"} */ import { registerServerReference } from "private-next-rsc-server-reference";
import { encryptActionBoundArgs, decryptActionBoundArgs } from "private-next-rsc-action-encryption";
import deleteFromDb from 'db';
export function Item({ id1, id2 }) {
    id1++;
    return (()=>{
        id1++;
        return <Button action={deleteItem}>Delete</Button>;
    })();
    var deleteItem = registerServerReference($$RSC_SERVER_ACTION_0, "6a88810ecce4a4e8b59d53b8327d7e98bbf251d7", null).bind(null, encryptActionBoundArgs("6a88810ecce4a4e8b59d53b8327d7e98bbf251d7", [
        id1,
        id2
    ]));
}
export async function $$RSC_SERVER_ACTION_0($$ACTION_CLOSURE_BOUND) {
    var [$$ACTION_ARG_0, $$ACTION_ARG_1] = await decryptActionBoundArgs("6a88810ecce4a4e8b59d53b8327d7e98bbf251d7", $$ACTION_CLOSURE_BOUND);
    await deleteFromDb($$ACTION_ARG_0);
    await deleteFromDb($$ACTION_ARG_1);
}
// In this example, if Button immediately executes the action, different ids should
// be passed.
export function Item2({ id1, id2 }) {
    id1++;
    const temp = [];
    temp.push(<Button action={deleteItem}>Delete</Button>);
    id1++;
    temp.push(<Button action={deleteItem}>Delete</Button>);
    return temp;
    var deleteItem = registerServerReference($$RSC_SERVER_ACTION_1, "90b5db271335765a4b0eab01f044b381b5ebd5cd", null).bind(null, encryptActionBoundArgs("90b5db271335765a4b0eab01f044b381b5ebd5cd", [
        id1,
        id2
    ]));
}
export async function $$RSC_SERVER_ACTION_1($$ACTION_CLOSURE_BOUND) {
    var [$$ACTION_ARG_0, $$ACTION_ARG_1] = await decryptActionBoundArgs("90b5db271335765a4b0eab01f044b381b5ebd5cd", $$ACTION_CLOSURE_BOUND);
    await deleteFromDb($$ACTION_ARG_0);
    await deleteFromDb($$ACTION_ARG_1);
}