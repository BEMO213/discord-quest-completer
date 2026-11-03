import { Game } from '@/types/types';
import { tryOnMounted, useAsyncState } from '@vueuse/core';
import { ref, watch } from 'vue';
import { message } from '@tauri-apps/plugin-dialog'; 
import { invoke } from '@tauri-apps/api/core';
import { useGlobalState } from './app-state';

export function useFetchGameList() {
    const { addLog } = useGlobalState();
    async function fetchGameListGHMirror() {
        addLog('Fetching game list from GitHub mirror...'); 
        const response = await invoke('fetch_gamelist_gh_mirror');
        return response as Game[] | unknown[] | undefined;
    }
    async function fetchGameListFromDiscord (){
        addLog('Fetching game list directly from discord...'); 
        const response = await invoke('fetch_gamelist_from_discord');
        return response as Game[] | unknown[] | undefined;
    };

    // const fetchBundledGameList = fetch(window.location.origin+'/gamelist.json', { method: 'GET' });

    const { 
        state: gameListGHMirror,
        error: errorGH,
        isReady: isReadyGH,
        execute: executeGH,
        isLoading: isLoadingGH
    } = useAsyncState<Game[] | unknown[] | undefined>(fetchGameListGHMirror, [], {
            immediate: false,
            resetOnExecute: true,
        });
    const { 
        state: gameListFromDiscord, 
        error: errorDiscord,
        isReady: isReadyDiscord,
        execute: executeDiscord,
        isLoading: isLoadingDiscord
    } = useAsyncState(fetchGameListFromDiscord, [], {
        immediate: false,
        resetOnExecute: true,
    });
    const { 
        state: bundledGameList,
        error: errorBundled,
        isReady: isReadyBundled,
        execute: executeBundled,
        isLoading: isLoadingBundled
    } = useAsyncState(() => {
        const result = import('../assets/gamelist.json').then(res=>res.default);
        addLog('Fetching bundled game list for fallback...');
        return result;
    }, [], {
        immediate: false,
        resetOnExecute: true,
    });

    const fetchError = ref<string | null>(null);

    const gameDB = ref<Game[]>([]);

    const allFetchDone = ref(false);

    function isValidGameList(data: unknown): data is Game[] {
        return Array.isArray(data) && data.length > 0 && data.every((game: any) =>
            game !== null &&
            typeof game === 'object' &&
            typeof game.id === 'string' &&
            game.id.length > 0 &&
            typeof game.name === 'string' &&
            game.name.length > 0 &&
            Array.isArray(game.executables) &&
            game.executables.every((executable: any) =>
                executable !== null &&
                typeof executable === 'object' &&
                typeof executable.name === 'string' &&
                executable.name.length > 0 &&
                typeof executable.os === 'string'
            ) &&
            (game.aliases === undefined ||
                (Array.isArray(game.aliases) && game.aliases.every((alias: any) => typeof alias === 'string')))
        );
    }

    watch(() => isReadyGH.value, async (newVal) => {
        addLog('debug','isReadyGH: ' + newVal); 
    });

    watch(() => isReadyDiscord.value, async (newVal) => {
        addLog('debug','isReadyDiscord: ' + newVal);
    })
    
    watch(() => isReadyBundled.value, async (newVal) => {
        addLog('debug','isReadyBundled: ' + newVal); 
    });

    let timeoutId: ReturnType<typeof setTimeout> | null = null;
    async function fetchGameList() {
        allFetchDone.value = false;
        fetchError.value = null;
        addLog('Fetching game list...');

        try {
            await Promise.all([executeGH(), executeBundled()]);
        } catch (error) {
            addLog('error', 'Error executing game-list fetch: ' + String(error));
        }

        const mirrorIsValid = isValidGameList(gameListGHMirror.value);
        if (!mirrorIsValid) {
            addLog('warning', 'GitHub mirror returned an unavailable or invalid game list; trying Discord.');
            try {
                await executeDiscord();
            } catch (error) {
                addLog('error', 'Error fetching game list from Discord: ' + String(error));
            }
        }

        const discordIsValid = isValidGameList(gameListFromDiscord.value);
        const bundledIsValid = isValidGameList(bundledGameList.value);

        if (mirrorIsValid) {
            gameDB.value = gameListGHMirror.value as Game[];
            addLog('Using game list from GitHub mirror. ' + gameDB.value.length + ' entries.');
        } else if (discordIsValid) {
            gameDB.value = gameListFromDiscord.value as Game[];
            addLog('Using game list from Discord. ' + gameDB.value.length + ' entries.');
        } else if (bundledIsValid) {
            gameDB.value = bundledGameList.value as Game[];
            addLog('Using bundled game list as fallback. ' + gameDB.value.length + ' entries.');
        } else {
            fetchError.value = 'All game-list sources returned unavailable or invalid data.';
            gameDB.value = [];
            addLog('error', fetchError.value);
            await message('There was an error fetching a valid game list. Check your connection and try again.', {
                title: 'Game List Fetch Error',
                kind: 'error',
                buttons: { ok: 'OK' }
            });
        }

        // Set a timeout to delay setting allFetchDone to true, to allow UI to update.
      
        timeoutId = setTimeout(() => {
            allFetchDone.value = true;
        }, 1800);

    }

    watch(allFetchDone, (newVal) => {
        if (newVal && timeoutId) {
            clearTimeout(timeoutId);
        }
    });

    tryOnMounted(async () => {
        await fetchGameList();
    });


    return {
        gameListGHMirror,
        gameListFromDiscord,
        bundledGameList,
        fetchError,
        isReadyGH,
        isReadyDiscord,
        isReadyBundled,
        gameDB,
        fetchGameList,
        isLoadingGH,
        isLoadingDiscord,
        isLoadingBundled,
        allFetchDone
    }
}