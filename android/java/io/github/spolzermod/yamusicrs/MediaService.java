package io.github.spolzermod.yamusicrs;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.content.pm.ServiceInfo;
import android.graphics.Bitmap;
import android.graphics.BitmapFactory;
import android.media.AudioAttributes;
import android.media.AudioFocusRequest;
import android.media.AudioManager;
import android.media.MediaMetadata;
import android.media.session.MediaSession;
import android.media.session.PlaybackState;
import android.os.Build;
import android.os.Bundle;
import android.os.Handler;
import android.os.IBinder;
import android.os.Looper;
import android.os.SystemClock;

/** Плеер в шторке и на экране блокировки. Пока играет музыка, держит процесс на переднем плане. */
public class MediaService extends Service {
    static final int PLAY = 0, PAUSE = 1, NEXT = 2, PREV = 3, LIKE = 4, DISLIKE = 5;

    static native void action(int code);

    static native void seek(long ms);

    private static final String CHANNEL = "playback";
    private static final int ID = 1;
    private static final Handler main = new Handler(Looper.getMainLooper());

    private static Context app;
    private static MediaSession session;
    private static Notification notification;
    private static MediaService running;
    private static boolean playing;
    private static AudioFocusRequest focus;
    private static boolean resumeOnGain;
    private static BroadcastReceiver noisy;

    /** Вызывается из Rust с любого потока. */
    public static void update(final Context ctx, final String title, final String artist, final byte[] cover,
            final long duration, final long position, final boolean isPlaying, final boolean liked) {
        main.post(new Runnable() {
            public void run() {
                show(ctx.getApplicationContext(), title, artist, cover, duration, position, isPlaying, liked);
            }
        });
    }

    public static void hide() {
        main.post(new Runnable() {
            public void run() {
                if (running != null) {
                    running.stopForeground(STOP_FOREGROUND_REMOVE);
                    running.stopSelf();
                }
                if (app != null) {
                    app.getSystemService(NotificationManager.class).cancel(ID);
                }
                if (session != null) {
                    session.release();
                    session = null;
                }
                notification = null;
                setPlaying(false);
            }
        });
    }

    private static void show(Context ctx, String title, String artist, byte[] cover, long duration, long position,
            boolean isPlaying, boolean liked) {
        app = ctx;
        if (session == null) {
            session = new MediaSession(ctx, "yamusic-rs");
            session.setCallback(new MediaSession.Callback() {
                public void onPlay() { action(PLAY); }
                public void onPause() { action(PAUSE); }
                public void onStop() { action(PAUSE); }
                public void onSkipToNext() { action(NEXT); }
                public void onSkipToPrevious() { action(PREV); }
                public void onSeekTo(long pos) { seek(pos); }
                public void onCustomAction(String a, Bundle extras) { action("like".equals(a) ? LIKE : DISLIKE); }
            });
            Intent open = ctx.getPackageManager().getLaunchIntentForPackage(ctx.getPackageName());
            session.setSessionActivity(PendingIntent.getActivity(ctx, 0, open, PendingIntent.FLAG_IMMUTABLE));
            session.setActive(true);
        }
        Bitmap art = cover == null ? null : BitmapFactory.decodeByteArray(cover, 0, cover.length);
        MediaMetadata.Builder meta = new MediaMetadata.Builder()
                .putString(MediaMetadata.METADATA_KEY_TITLE, title)
                .putString(MediaMetadata.METADATA_KEY_ARTIST, artist)
                .putLong(MediaMetadata.METADATA_KEY_DURATION, duration);
        if (art != null) {
            meta.putBitmap(MediaMetadata.METADATA_KEY_ALBUM_ART, art);
        }
        session.setMetadata(meta.build());
        session.setPlaybackState(new PlaybackState.Builder()
                .setActions(PlaybackState.ACTION_PLAY | PlaybackState.ACTION_PAUSE | PlaybackState.ACTION_PLAY_PAUSE
                        | PlaybackState.ACTION_SKIP_TO_NEXT | PlaybackState.ACTION_SKIP_TO_PREVIOUS
                        | PlaybackState.ACTION_SEEK_TO | PlaybackState.ACTION_STOP)
                .setState(isPlaying ? PlaybackState.STATE_PLAYING : PlaybackState.STATE_PAUSED, position,
                        isPlaying ? 1f : 0f, SystemClock.elapsedRealtime())
                .addCustomAction(new PlaybackState.CustomAction.Builder("dislike", "Не нравится",
                        R.drawable.ic_dislike).build())
                .addCustomAction(new PlaybackState.CustomAction.Builder("like", liked ? "Убрать из «Мне нравится»" : "Нравится",
                        liked ? R.drawable.ic_liked : R.drawable.ic_like).build())
                .build());

        NotificationManager nm = ctx.getSystemService(NotificationManager.class);
        if (nm.getNotificationChannel(CHANNEL) == null) {
            NotificationChannel ch = new NotificationChannel(CHANNEL, "Плеер", NotificationManager.IMPORTANCE_LOW);
            ch.setShowBadge(false);
            nm.createNotificationChannel(ch);
        }
        // До Android 13 кнопки берутся из уведомления, а не из сессии.
        Notification.Builder b = new Notification.Builder(ctx, CHANNEL)
                .setSmallIcon(R.drawable.ic_star)
                .setContentTitle(title)
                .setContentText(artist)
                .setLargeIcon(art)
                .setContentIntent(session.getController().getSessionActivity())
                .setVisibility(Notification.VISIBILITY_PUBLIC)
                .setShowWhen(false)
                .setOngoing(isPlaying)
                .addAction(button(ctx, R.drawable.ic_prev, "Предыдущий", PREV))
                .addAction(isPlaying ? button(ctx, R.drawable.ic_pause, "Пауза", PAUSE)
                        : button(ctx, R.drawable.ic_play, "Слушать", PLAY))
                .addAction(button(ctx, R.drawable.ic_next, "Следующий", NEXT))
                .addAction(button(ctx, liked ? R.drawable.ic_liked : R.drawable.ic_like, "Нравится", LIKE))
                .setStyle(new Notification.MediaStyle()
                        .setMediaSession(session.getSessionToken())
                        .setShowActionsInCompactView(0, 1, 2));
        notification = b.build();

        if (isPlaying && running == null) {
            ctx.startForegroundService(new Intent(ctx, MediaService.class));
        } else if (isPlaying) {
            running.foreground();
        } else {
            if (running != null) {
                running.stopForeground(STOP_FOREGROUND_DETACH);
            }
            nm.notify(ID, notification);
        }
        setPlaying(isPlaying);
    }

    private static Notification.Action button(Context ctx, int icon, String title, int code) {
        Intent i = new Intent(ctx, MediaService.class).setAction(String.valueOf(code));
        PendingIntent pi = PendingIntent.getForegroundService(ctx, code, i, PendingIntent.FLAG_IMMUTABLE);
        return new Notification.Action.Builder(android.graphics.drawable.Icon.createWithResource(ctx, icon), title, pi).build();
    }

    /** Фокус звука и «шумный» выход: звонок ставит паузу, выдернутые наушники тоже. */
    private static void setPlaying(boolean now) {
        if (now == playing || app == null) {
            return;
        }
        playing = now;
        AudioManager am = app.getSystemService(AudioManager.class);
        if (now) {
            resumeOnGain = false;
            if (focus == null) {
                focus = new AudioFocusRequest.Builder(AudioManager.AUDIOFOCUS_GAIN)
                        .setAudioAttributes(new AudioAttributes.Builder()
                                .setUsage(AudioAttributes.USAGE_MEDIA)
                                .setContentType(AudioAttributes.CONTENT_TYPE_MUSIC)
                                .build())
                        .setWillPauseWhenDucked(false)
                        .setOnAudioFocusChangeListener(new AudioManager.OnAudioFocusChangeListener() {
                            public void onAudioFocusChange(int change) {
                                if (change == AudioManager.AUDIOFOCUS_LOSS) {
                                    action(PAUSE);
                                } else if (change == AudioManager.AUDIOFOCUS_LOSS_TRANSIENT) {
                                    resumeOnGain = playing;
                                    action(PAUSE);
                                } else if (change == AudioManager.AUDIOFOCUS_GAIN && resumeOnGain) {
                                    resumeOnGain = false;
                                    action(PLAY);
                                }
                            }
                        }, main)
                        .build();
            }
            am.requestAudioFocus(focus);
            if (noisy == null) {
                noisy = new BroadcastReceiver() {
                    public void onReceive(Context c, Intent i) { action(PAUSE); }
                };
                app.registerReceiver(noisy, new IntentFilter(AudioManager.ACTION_AUDIO_BECOMING_NOISY));
            }
        } else if (!resumeOnGain) {
            if (focus != null) {
                am.abandonAudioFocusRequest(focus);
            }
        }
        if (!now && noisy != null) {
            app.unregisterReceiver(noisy);
            noisy = null;
        }
    }

    private void foreground() {
        if (notification == null) {
            return;
        }
        if (Build.VERSION.SDK_INT >= 29) {
            startForeground(ID, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK);
        } else {
            startForeground(ID, notification);
        }
    }

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        running = this;
        // Система ждёт startForeground в ответ на startForegroundService, даже если это пауза.
        foreground();
        if (!playing) {
            stopForeground(STOP_FOREGROUND_DETACH);
        }
        String a = intent == null ? null : intent.getAction();
        if (a != null) {
            action(Integer.parseInt(a));
        }
        return START_NOT_STICKY;
    }

    // Окно Slint поднимается раз на процесс, поэтому смахнутое приложение закрываем целиком.
    @Override
    public void onTaskRemoved(Intent root) {
        android.os.Process.killProcess(android.os.Process.myPid());
    }

    @Override
    public void onDestroy() {
        running = null;
    }

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }
}
